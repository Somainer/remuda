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

/// Tolerance band on the estimated hard cap: warn at estimate, stop at 1.15×.
pub const BUDGET_STOP_FACTOR: f64 = 1.15;

/// One persisted usage event projection.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageEventRow {
    /// Owning instance.
    pub instance_id: String,
    /// Journal seq (dedupe key with instance).
    pub seq: i64,
    /// Provider profile id when the launch resolved one (`pvp_…`).
    pub profile_id: Option<String>,
    /// Model id from the instance spec.
    pub model: Option<String>,
    /// Usage scope (`turn` / `session` / …).
    pub scope: String,
    /// Native scope id (the assistant message id for a turn snapshot; the
    /// per-response id for a codex Message row). Drives durable re-hydration
    /// dedupe and the context basket.
    pub scope_id: Option<String>,
    /// Native turn id this row belongs to (codex Message rows carry it on the
    /// stamped observation; r6 item 5). `None` for legacy rows and scope
    /// kinds that already key on the turn (Claude Turn scope_id).
    pub turn_id: Option<String>,
    /// Snapshot revision for a given `(scope, scope_id)`. Turn observations
    /// are immutable (always 1); Grok/Codex Session snapshots keep a stable
    /// session id and re-emit with an increasing revision, so a newer
    /// revision replaces the older row instead of being frozen.
    pub metric_revision: i64,
    /// Native session id qualifying [`Self::metric_revision`]. The revision is
    /// a record position WITHIN one native session transcript, so the turn
    /// authority watermark is `(source_session_id, metric_revision)`: two
    /// sessions both replaying from position 1 must not freeze each other.
    /// `None` is the legacy unqualified lineage (pre-r15 rows and producers
    /// that do not report a session) (c-ctxusage r15 item 4).
    pub source_session_id: Option<String>,
    /// Snapshot vs delta.
    pub mode: String,
    /// Total tokens when known.
    pub total_tokens: Option<i64>,
    /// Fresh (uncached) input tokens when known.
    pub input_tokens: Option<i64>,
    /// Output tokens when known.
    pub output_tokens: Option<i64>,
    /// Prompt-cache read tokens when known.
    pub cache_read_tokens: Option<i64>,
    /// Prompt-cache creation (write) tokens when known.
    pub cache_write_tokens: Option<i64>,
    /// Estimated/reported cost decimal string ("USD") when known.
    pub cost_usd: Option<String>,
    /// `reported` / `estimated`.
    pub accounting: String,
    /// Observed-at timestamp.
    pub observed_at: String,
    /// Whether `observed_at` came from the source's native timestamp
    /// (`"native"`), was stamped at journal ingest (`"ingest"` — a
    /// timestamp-free historical replay, NEVER time-authoritative), or was
    /// defaulted by the migration on a database from BEFORE this column
    /// existed (`"legacy"` — no provenance existed at all, so the retained
    /// timestamp is trusted as the only evidence, c-usagefu r8 item 5; r37
    /// item 3). Only a native timestamp may authoritatively repair the
    /// stored time; ingest fallbacks never overwrite one (c-ctxusage r4
    /// item 5).
    pub observed_at_source: String,
    /// Native per-model context window the harness reported (`result`/
    /// `modelUsage.contextWindow`, via `UsagePayload.contextWindow`). The
    /// newest reported window wins (c-usagefu (c)).
    pub native_context_window: Option<i64>,
}

/// Create the `usage_events` table. Idempotent and safe against every prior
/// schema: the `scope_id` partial unique index is created only AFTER the
/// column is guaranteed to exist, so a pre-delivery database (no scope_id
/// column) upgrades instead of failing to open.
///
/// c-usagefu r14 item 1 (Ruling M1): a usage migration must NEVER make the
/// Hub unable to start. The ENTIRE migration — table/column DDL, the
/// scope-id journal backfill, duplicate consolidation and every unique
/// index — runs in ONE transaction; any error rolls ALL of it back to the
/// previously-openable shape. SQLite DDL is transactional, so an
/// `ALTER TABLE`/`CREATE INDEX` that never committed is gone too. The
/// caller (the store opener) logs a migration error and opens on the old
/// shape instead of propagating it.
pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    run_migrations(&tx)?;
    tx.commit()
}

/// Persisted marker table: records which ONE-TIME data migrations have
/// completed, so a later start never repeats a step that would corrupt the
/// final shape (c-usagefu r15 item 2).
const META_DDL: &str = "CREATE TABLE IF NOT EXISTS usage_schema_meta (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
     );";

/// Marker for the scope-id journal backfill + duplicate consolidation
/// (c-usagefu r13/r14). Once recorded, the disagreeing-bucket rows were
/// intentionally cleared back to NULL and the unique indexes exist; running
/// the journal backfill again would RE-CREATE those scope ids straight into
/// the unique index and fail every restart.
const MARKER_SCOPE_CONSOLIDATION: &str = "scope_consolidation_v1";

/// Test-only sentinel table: while it exists, the usage migration fails AFTER
/// its additive DDL, inside the migration transaction — a deterministic,
/// per-database stand-in for an exhausted disk / SQL error that rolls the
/// whole migration back (c-usagefu r15 item 1). Not a global flag, so parallel
/// tests opening their own stores are unaffected.
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

/// Columns the final schema of THIS delivery carries, in the order the
/// migration adds them. The migration is ONE atomic transaction (Ruling M1),
/// so a committed migration leaves all of these present and a rolled-back
/// migration leaves the subset the OPENING release had — checking the whole
/// set recognises a failed upgrade from the usagefu main-parent shape
/// (cbac58e1), which already had `scope_id` but not the lineage columns
/// (r37 item 4).
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

/// Whether the OPEN database carries the final usage schema. A failed
/// migration rolls the DDL back too, leaving the previously-openable shape —
/// reads must then degrade to Unknown instead of issuing SQL that references
/// missing columns (c-usagefu r15 item 1; r37 item 4: the probe must check
/// the COMPLETE required schema, not only `scope_id`). A missing
/// `usage_events` table (nothing ever ingested, migration rolled back) is the
/// same legacy shape.
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

/// All usage migration steps, in order. Runs inside [`migrate`]'s single
/// transaction: every helper here must do its work on `conn` WITHOUT opening
/// a nested transaction.
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
    // c-ctxusage × c-usagefu merge: provenance tag separating rows that
    // already existed at this migration (pre-upgrade producers, including
    // rows written while an earlier migration window was rolled back and
    // recovered from the journal below) from rows projected LIVE after the
    // upgrade. The fold treats an unprovable LEGACY turn next to scoped stock
    // as conservative Unknown (c-ctxusage r29 ambiguity), while a live turn
    // the current codex adapter projected follows Ruling B flow billing even
    // beside stock; the two shapes are otherwise identical, so without the
    // tag the two rulings cannot be distinguished. Existing rows are tagged
    // only on the migration that INTRODUCES the column: on a later restart the
    // column already exists and post-upgrade live rows (NULL) stay live.
    let tag_legacy_rows = !crate::store::column_exists(conn, "usage_events", "lineage_state")?;
    crate::store::ensure_column(conn, "usage_events", "lineage_state", "TEXT")?;
    if tag_legacy_rows {
        conn.execute("UPDATE usage_events SET lineage_state = 'legacy'", [])?;
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
    // unique indexes MUST be dropped BEFORE any journal backfill stamps
    // scope_id on previously-NULL rows — stamping two rows with one id under
    // the old (instance_id, scope, scope_id) index aborts the migration.
    tx.execute_batch("DROP INDEX IF EXISTS usage_events_scope_dedupe;")?;
    tx.execute_batch("DROP INDEX IF EXISTS usage_events_turn_dedupe;")?;
    // c-usagefu r13/r16: recover scope_id and the fallback-only projection
    // fields (native turn id, native time, revision, context window) from the
    // retained journal BEFORE the lineage collapse below. Rows written through
    // the legacy path while a migration stayed rolled back persist only the 14
    // legacy columns even when the newer columns already committed, so these
    // run on every restart; every assignment fills only missing values and the
    // revision is raise-only (r38 item 1), so re-projection can never lower a
    // revision a live write already established.
    backfill_scope_ids_from_journal(tx)?;
    reconstruct_fallback_projection_from_journal(tx)?;
    // c-ctxusage r9/r29: session-scope then turn-scope identity/lineage from
    // retained evidence (the scope backfill above is the same idempotent
    // update for scope ids; these also stamp session scope_id and the
    // turn→native-session lineage). They prove NULL-scope rows with retained
    // journal evidence on every migration, including rows written through the
    // fallback path after the schema committed.
    backfill_legacy_scope_ids(tx)?;
    backfill_legacy_turn_identity(tx)?;
    // c-usagefu r14 (M1): journal-recovered turn rows that share a key but
    // disagree on counters are two degenerate-id calls from the HISTORICAL
    // unproven producer, not one re-tail — return them to the legacy
    // NULL-scope path before the fold winner is collapsed. Kept unconditional
    // so rows recovered by the backfills above on any restart are classified;
    // r38 item 2 tightens its scope to the historical producer so proven
    // lineage recovery rows resolve through revision authority instead.
    clear_disagreeing_recovered_turn_keys(tx)?;
    // r30: identity UPDATE can merge a legacy turn with a later qualified
    // replay; collapse to the fold winner BEFORE identical-counter
    // consolidation (which keeps the oldest row for stocks).
    collapse_duplicate_turn_lineage_rows(tx)?;
    // c-usagefu: identical Message duplicates collapse to highest seq.
    consolidate_message_duplicates(tx)?;
    consolidate_identical_scoped_rows(tx)?;
    // r11 item 1: replay the cumulative-growth acceptance rule for imported /
    // pre-acceptance chains before rowid order means accepted history.
    normalise_scoped_session_chains(tx)?;
    // r16 item 3 (T1'): recreate the per-LINEAGE turn unique index.
    tx.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS usage_events_turn_dedupe
             ON usage_events(
                 instance_id, scope, scope_id,
                 COALESCE(source_session_id, '')
             )
             WHERE scope = 'turn' AND scope_id IS NOT NULL;",
    )?;
    // The outer `migrate` owns the single transaction and commits it.
    // Record completion for usagefu r15's one-time marker (no-op on older paths).
    set_meta_marker(conn, MARKER_SCOPE_CONSOLIDATION)?;
    Ok(())
}

/// Backfill pre-`scopeId` TURN rows from their retained usage observations
/// (c-ctxusage r29 item 1).
///
/// Identity (`scope_id`) is the payload's turn message id. Lineage
/// (`source_session_id`) comes from the observation envelope's
/// `source.nativeSessionId`, which the driver stamps only under a CONFIRMED
/// session binding; when `nativeTurnId` is known it must equal the payload
/// turn id, so a stamp for another turn proves nothing. Rows the journal
/// cannot qualify stay NULL and are treated as ambiguous (Unknown totals) by
/// the folds, never guessed and never silently dropped.
fn backfill_legacy_turn_identity(conn: &Connection) -> rusqlite::Result<()> {
    let has_journal: bool = conn.query_row(
        "SELECT EXISTS (
            SELECT 1 FROM sqlite_master
            WHERE type = 'table' AND name = 'journal'
        )",
        [],
        |row| row.get(0),
    )?;
    if !has_journal {
        return Ok(());
    }
    // 1) Turn scope id from the payload.
    conn.execute(
        "UPDATE usage_events AS u
         SET scope_id = (
             SELECT json_extract(j.payload_json, '$.payload.scopeId')
             FROM journal AS j
             WHERE j.instance_id = u.instance_id
               AND j.seq = u.seq
               AND json_valid(j.payload_json)
               AND json_extract(j.payload_json, '$.kind') = 'usage'
               AND json_extract(j.payload_json, '$.payload.scope') = 'turn'
               AND json_type(j.payload_json, '$.payload.scopeId') = 'text'
               AND length(json_extract(j.payload_json, '$.payload.scopeId')) > 0
         )
         WHERE u.scope = 'turn' AND u.scope_id IS NULL
           AND EXISTS (
             SELECT 1 FROM journal AS j
             WHERE j.instance_id = u.instance_id
               AND j.seq = u.seq
               AND json_valid(j.payload_json)
               AND json_extract(j.payload_json, '$.kind') = 'usage'
               AND json_extract(j.payload_json, '$.payload.scope') = 'turn'
               AND json_type(j.payload_json, '$.payload.scopeId') = 'text'
               AND length(json_extract(j.payload_json, '$.payload.scopeId')) > 0
         )",
        [],
    )?;
    // 2) Native-session lineage from the confirmed-binding envelope stamp.
    conn.execute(
        "UPDATE usage_events AS u
         SET source_session_id = (
             SELECT json_extract(j.payload_json, '$.source.nativeSessionId.value')
             FROM journal AS j
             WHERE j.instance_id = u.instance_id
               AND j.seq = u.seq
               AND json_valid(j.payload_json)
               AND json_extract(j.payload_json, '$.kind') = 'usage'
               AND json_extract(j.payload_json, '$.payload.scope') = 'turn'
               AND json_type(j.payload_json, '$.source.nativeSessionId') = 'object'
               AND json_extract(j.payload_json, '$.source.nativeSessionId.state') = 'known'
               AND json_type(j.payload_json, '$.source.nativeSessionId.value') = 'text'
               AND length(json_extract(j.payload_json, '$.source.nativeSessionId.value')) > 0
               AND (
                   json_type(j.payload_json, '$.source.nativeTurnId') <> 'object'
                   OR json_extract(j.payload_json, '$.source.nativeTurnId.state') <> 'known'
                   OR json_extract(j.payload_json, '$.source.nativeTurnId.value')
                      = json_extract(j.payload_json, '$.payload.scopeId')
               )
         )
         WHERE u.scope = 'turn' AND u.source_session_id IS NULL
           AND EXISTS (
             SELECT 1 FROM journal AS j
             WHERE j.instance_id = u.instance_id
               AND j.seq = u.seq
               AND json_valid(j.payload_json)
               AND json_extract(j.payload_json, '$.kind') = 'usage'
               AND json_extract(j.payload_json, '$.payload.scope') = 'turn'
               AND json_type(j.payload_json, '$.source.nativeSessionId') = 'object'
               AND json_extract(j.payload_json, '$.source.nativeSessionId.state') = 'known'
               AND json_type(j.payload_json, '$.source.nativeSessionId.value') = 'text'
               AND length(json_extract(j.payload_json, '$.source.nativeSessionId.value')) > 0
               AND (
                   json_type(j.payload_json, '$.source.nativeTurnId') <> 'object'
                   OR json_extract(j.payload_json, '$.source.nativeTurnId.state') <> 'known'
                   OR json_extract(j.payload_json, '$.source.nativeTurnId.value')
                      = json_extract(j.payload_json, '$.payload.scopeId')
               )
         )",
        [],
    )?;
    Ok(())
}

/// Backfill pre-`scopeId` NULL-scope session rows from the matching retained
/// journal payloads (c-ctxusage r9 item 5). A legacy row's identity is
/// provable when the journal's `(instance_id, seq)` event is a usage event
/// whose `payload.scopeId` is a non-empty string; that id is stamped onto the
/// row so the stock joins the scoped totals instead of vanishing on the first
/// scoped point. No-op when the database has no `journal` table (standalone
/// usage-store migrations).
fn backfill_legacy_scope_ids(conn: &Connection) -> rusqlite::Result<()> {
    let has_journal: bool = conn.query_row(
        "SELECT EXISTS (
            SELECT 1 FROM sqlite_master
            WHERE type = 'table' AND name = 'journal'
        )",
        [],
        |row| row.get(0),
    )?;
    if !has_journal {
        return Ok(());
    }
    conn.execute(
        "UPDATE usage_events AS u
         SET scope_id = (
             SELECT json_extract(j.payload_json, '$.payload.scopeId')
             FROM journal AS j
             WHERE j.instance_id = u.instance_id
               AND j.seq = u.seq
               AND json_valid(j.payload_json)
               AND json_extract(j.payload_json, '$.kind') = 'usage'
               AND json_extract(j.payload_json, '$.payload.scope') = 'session'
               AND json_type(j.payload_json, '$.payload.scopeId') = 'text'
               AND length(json_extract(j.payload_json, '$.payload.scopeId')) > 0
         )
         WHERE u.scope = 'session' AND u.scope_id IS NULL
           AND EXISTS (
             SELECT 1 FROM journal AS j
             WHERE j.instance_id = u.instance_id
               AND j.seq = u.seq
               AND json_valid(j.payload_json)
               AND json_extract(j.payload_json, '$.kind') = 'usage'
               AND json_extract(j.payload_json, '$.payload.scope') = 'session'
               AND json_type(j.payload_json, '$.payload.scopeId') = 'text'
               AND length(json_extract(j.payload_json, '$.payload.scopeId')) > 0
         )",
        [],
    )?;
    Ok(())
}

/// Return journal-recovered `turn` rows that share a key but disagree on
/// counters to the legacy NULL-scope path before the fold winner is chosen
/// (c-usagefu r14 item 1, Ruling M1; r37 item 1).
///
/// origin/main wrote codex Turn rows without persisting `scope_id`, and the
/// backfill above recovers it from the durable journal. Two genuinely
/// different result frames can recover the SAME id while reporting DIFFERENT
/// counters (degenerate ids, e.g. both mapped to `"0"`): they are two calls,
/// nothing is merged, and every row of such a group has its `scope_id`
/// cleared back to NULL so each keeps counting once on the legacy NULL-scope
/// path instead of collapsing into one fold winner. A byte-zero re-tail with
/// IDENTICAL buckets is the same call, but nothing here has to remove it:
/// [`collapse_duplicate_turn_lineage_rows`] keeps exactly one fold winner per
/// lineage for every same-key group regardless of counters.
///
/// Siblings are matched on the FULL authority key
/// `(instance_id, scope_id, source_session_id)`: two rows for one turn in
/// DIFFERENT proven native-session lineages are both retained watermarks
/// (one revision-replaced row per lineage, c-ctxusage r16 item 3), never
/// degenerate duplicates of each other — this UPDATE can never clear one
/// because of another session's row. NULL sessions match NULL sessions; the
/// unqualified legacy lineage is its own key.
///
/// Agreement is on the five TOKEN buckets only, compared NULL-safely with
/// `IS` (a byte-zero re-tail carries the identical sparse bucket set). Cost
/// is a priced projection, never a call identity. Runs inside the migration
/// transaction on EVERY migration: the journal backfills re-stamp
/// intentionally-cleared ids unconditionally (fallback-period rows must
/// recover on any restart), so the clear is re-applied right after them. It
/// is idempotent — a settled database has at most one physical row per
/// lineage key, so the EXISTS match finds nothing.
fn clear_disagreeing_recovered_turn_keys(conn: &Connection) -> rusqlite::Result<()> {
    // r38 item 2: only the HISTORICAL unqualified lineage (no proven session)
    // is treated as degenerate-id calls. Successive snapshots journal recovery
    // stamps with a confirmed common source_session_id are revisions of one
    // turn even when counters differ; they keep scope/lineage and resolve to
    // the fold winner, so the outer row and every sibling must be NULL.
    conn.execute(
        "UPDATE usage_events
         SET scope_id = NULL
         WHERE scope = 'turn' AND scope_id IS NOT NULL
           AND source_session_id IS NULL
           AND EXISTS (
               SELECT 1 FROM usage_events d
               WHERE d.instance_id = usage_events.instance_id
                 AND d.scope = 'turn'
                 AND d.scope_id = usage_events.scope_id
                 AND d.source_session_id IS NULL
                 AND d.rowid <> usage_events.rowid
                 AND (d.total_tokens     IS NOT usage_events.total_tokens
                   OR d.input_tokens     IS NOT usage_events.input_tokens
                   OR d.output_tokens    IS NOT usage_events.output_tokens
                   OR d.cache_read_tokens  IS NOT usage_events.cache_read_tokens
                   OR d.cache_write_tokens IS NOT usage_events.cache_write_tokens))",
        [],
    )?;
    Ok(())
}

/// Within one turn lineage `(instance, scope_id, COALESCE(source_session_id,
/// '')) keep the same WINNER the read folds pick — the newest accepted frame
/// (`seq DESC, rowid DESC`, matching `WINNING_TURN_ROWS_SQL`); rows in
/// DIFFERENT native-session lineages for the same scope id are retained —
/// each lineage keeps its own revision-replaced row (c-ctxusage r16 item 3).
///
/// The newest frame is the authority: a resume replay (r30 item 1) is how a
/// pre-scopeId legacy turn and its later qualified frame become one lineage,
/// and the replay carries the native timestamp/revision the read model uses;
/// keeping the older durable row would silently change the fold and the
/// native-time windows.
/// One turn frame participating in reconciliation — either the live lineage
/// row plus an incoming frame ([`upsert_turn_snapshot`]) or the ordered
/// recovered rows of one lineage (migration collapse). The reconciliation rule
/// is [`reconcile_turn_frame`] in BOTH directions: r40 AB1 leaves exactly one
/// turn/session reconcile code path for live insert, migration, recovery and
/// fallback.
#[derive(Clone, Debug)]
struct TurnFrame {
    /// Durable row id when this frame is already persisted (the stored live
    /// row, or a recovered row); the reconciled result targets the newest
    /// authority frame's row id.
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
}

/// Reconcile an AUTHORITATIVELY-NEWER incoming frame over the retained frame,
/// returning the frame the surviving row must hold. The CALLER guarantees
/// authority:
///
/// * live insertion — a strictly higher `metric_revision` within the lineage
///   (the stale-position gate runs before this; equal/older positions change
///   nothing and never reach here);
/// * migration collapse — revision DESC, then seq/rowid DESC (seq breaks ties
///   of equal revision for imported duplicate rows, the AA1 rule).
///
/// Two channels, exactly the live semantics documented on
/// [`upsert_turn_snapshot`]:
///
/// * a STRICTLY-NEWER COST-ONLY frame (no token basket reported) is a billing
///   correction: it advances sequence/revision/lineage and the bill (an
///   unreported bill keeps the stored one) but carries ALL five counters — a
///   known total over still-incomplete baskets included — and the turn clock
///   forward UNCHANGED, along with profile/model/mode/accounting;
/// * a frame reporting counters merges basket gaps with [`SnapshotCounters::merged_over`],
///   takes the bill unless it omitted one, follows the newer snapshot's clock
///   (an ingest fallback never overwrites a retained native time), and carries
///   profile/model/mode/accounting.
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
    if incoming.observed_at_source == "native" || stored.observed_at_source != "native" {
        reconciled.observed_at = incoming.observed_at.clone();
        reconciled.observed_at_source = incoming.observed_at_source.clone();
    }
    reconciled
}

/// Persist a reconciled frame onto its target row (the newest authority
/// frame's row id), scoped by rowid — never by the nullable
/// `(instance, scope_id)` key.
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
            observed_at_source = ?15
         WHERE rowid = ?16",
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
            rowid,
        ],
    )?;
    Ok(())
}

/// Within one turn lineage `(instance_id, scope_id, COALESCE(source_session_id,
/// '')) reconcile duplicate recovered rows using EXACTLY the live insertion
/// rule (r39 item 1 / AA1; r40 AB1 — the shared
/// [`reconcile_turn_frame`]/[`persist_reconciled_turn`] path, so a cost-only
/// recovery carries counters and the native clock exactly like a live
/// billing correction):
///
/// * the surviving row is the strictly-highest `metric_revision`; `seq` (then
///   rowid) only breaks ties of EQUAL revision — a later-ingestion STALE replay
///   at a lower revision never wins (the read folds and live upsert then agree,
///   so the established watermark cannot regress);
/// * buckets/cost merge into the winner the way the live upsert does: a value
///   the winner reported wins, and a bucket the newer winner did NOT report is
///   retained from the best lower-revision row (a newer partial frame never
///   erases a known bucket); a winner-unreported total derives from the merged
///   all-known basket (U1);
/// * an ingest clock never replaces a retained native timestamp.
fn collapse_duplicate_turn_lineage_rows(conn: &Connection) -> rusqlite::Result<usize> {
    let select = "SELECT rowid, seq, profile_id, model, mode, metric_revision,
                         source_session_id, total_tokens, input_tokens, output_tokens,
                         cache_read_tokens, cache_write_tokens, cost_usd, accounting,
                         observed_at, observed_at_source
                  FROM usage_events
                  WHERE scope = 'turn' AND scope_id IS NOT NULL
                  ORDER BY instance_id, scope_id, COALESCE(source_session_id, ''),
                           metric_revision DESC, seq DESC, rowid DESC";
    let map_frame = |r: &rusqlite::Row<'_>| {
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
        })
    };
    let mut stmt = conn.prepare(select)?;
    let ordered: Vec<TurnFrame> = stmt
        .query_map([], map_frame)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    let mut keys = conn.prepare(
        "SELECT instance_id, scope_id, COALESCE(source_session_id, '')
         FROM usage_events
         WHERE scope = 'turn' AND scope_id IS NOT NULL
         ORDER BY instance_id, scope_id, COALESCE(source_session_id, ''),
                  metric_revision DESC, seq DESC, rowid DESC",
    )?;
    let keyed: Vec<(String, String, String)> = keys
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(keys);

    let mut deleted = 0usize;
    let mut cur: Option<(String, String, String)> = None;
    let mut group: Vec<TurnFrame> = Vec::new();
    for (row, key) in ordered.into_iter().zip(keyed) {
        if Some(&key) != cur.as_ref() {
            deleted += collapse_one_turn_lineage(conn, &mut group)?;
            cur = Some(key);
        }
        group.push(row);
    }
    deleted += collapse_one_turn_lineage(conn, &mut group)?;
    Ok(deleted)
}

/// Merge one authority-ordered lineage group (winner first, losers following)
/// into the winner row via the shared [`reconcile_turn_frame`] rule, then
/// delete the losers. Returns the deleted count.
fn collapse_one_turn_lineage(
    conn: &Connection,
    group: &mut Vec<TurnFrame>,
) -> rusqlite::Result<usize> {
    if group.len() <= 1 {
        group.clear();
        return Ok(0);
    }
    let all_rowids: Vec<i64> = group.iter().map(|f| f.rowid.unwrap()).collect();
    // Fold the LOWEST-authority frame up to the winner, so each step applies
    // the same "newer frame over retained frame" reconciliation the live path
    // runs. The final target is the winner's own row id.
    let mut ascending = group.drain(..).rev();
    let mut reconciled = ascending.next().unwrap();
    for newer in ascending {
        reconciled = reconcile_turn_frame(&reconciled, &newer);
    }
    let target = reconciled.rowid.unwrap();
    persist_reconciled_turn(conn, &reconciled)?;
    let losers: Vec<i64> = all_rowids
        .into_iter()
        .filter(|rowid| *rowid != target)
        .collect();
    let n = losers.len();
    conn.execute(
        &format!(
            "DELETE FROM usage_events WHERE rowid IN ({})",
            losers.iter().map(|_| "?").collect::<Vec<_>>().join(",")
        ),
        rusqlite::params_from_iter(losers),
    )?;
    Ok(n)
}

/// Collapse rows that became exact duplicates after the retained-journal
/// scope-id backfill: same `(instance_id, scope, scope_id)` AND identical
/// counters in every bucket are the same cumulative point replayed at another
/// journal seq, so keeping both would double-count a stock. Turn rows in
/// DIFFERENT native-session lineages are never duplicates even with matching
/// counters — each lineage keeps its own row (c-ctxusage r16 item 3). The
/// oldest durable identity (smallest rowid) wins; its `observed_at` is
/// retained. Runs inside [`migrate`]'s transaction (c-ctxusage r10 item 2).
fn consolidate_identical_scoped_rows(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM usage_events
         WHERE scope_id IS NOT NULL
           AND rowid NOT IN (
               SELECT MIN(rowid)
               FROM usage_events
               WHERE scope_id IS NOT NULL
               GROUP BY instance_id, scope, scope_id,
                        CASE WHEN scope = 'turn'
                             THEN COALESCE(source_session_id, '')
                             ELSE '' END,
                        COALESCE(total_tokens, -1),
                        COALESCE(input_tokens, -1),
                        COALESCE(output_tokens, -1),
                        COALESCE(cache_read_tokens, -1),
                        COALESCE(cache_write_tokens, -1)
           )",
        [],
    )
}

/// Replay the cumulative-growth acceptance rule over every proven scoped
/// session chain and drop points the rule would have rejected at insert.
///
/// Imported legacy rows (and rows written before the content-ordered rule
/// existed) can land out of cumulative order: a high-water 200/20 row at a
/// small rowid followed by a stale historical replay 100/10 at a larger one
/// leaves raw rowid order disagreeing with the floors — current-stock
/// selection would show 100/10 while the retained 200/20 still freezes later
/// points. Walking each `(instance_id, scope_id)` chain in rowid order with
/// the live [`freezes_against`] rule removes exactly those points, so rowid
/// order is once more a true accepted-history order. Idempotent: an already
/// normalised chain deletes nothing (c-ctxusage r11 item 1).
fn normalise_scoped_session_chains(conn: &Connection) -> rusqlite::Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT rowid, instance_id, scope_id, total_tokens, input_tokens,
                output_tokens, cache_read_tokens, cache_write_tokens, cost_usd
         FROM usage_events
         WHERE scope = 'session' AND scope_id IS NOT NULL
         ORDER BY instance_id ASC, scope_id ASC, rowid ASC",
    )?;
    let rows = stmt.query_map([], |r| {
        let rowid: i64 = r.get(0)?;
        let instance_id: String = r.get(1)?;
        let scope_id: String = r.get(2)?;
        let point = SessionStockPoint {
            counters: SnapshotCounters {
                total: r.get(3)?,
                input: r.get(4)?,
                output: r.get(5)?,
                cache_read: r.get(6)?,
                cache_write: r.get(7)?,
            },
            cost: parse_cost(r.get::<_, Option<String>>(8)?),
        };
        Ok((rowid, instance_id, scope_id, point))
    })?;
    let mut current_key: Option<(String, String)> = None;
    let mut accepted: Vec<SessionStockPoint> = Vec::new();
    let mut stale_rowids: Vec<i64> = Vec::new();
    for entry in rows {
        let (rowid, instance_id, scope_id, point) = entry?;
        let key = Some((instance_id, scope_id));
        if key != current_key {
            current_key = key;
            accepted.clear();
        }
        if freezes_against(&point, &accepted) {
            stale_rowids.push(rowid);
        } else {
            accepted.push(point);
        }
    }
    drop(stmt);
    let mut removed = 0;
    for rowid in stale_rowids {
        removed += conn.execute("DELETE FROM usage_events WHERE rowid = ?1", params![rowid])?;
    }
    Ok(removed)
}

fn backfill_scope_ids_from_journal(conn: &Connection) -> rusqlite::Result<()> {
    // The migration runs on the store connection; unit-test fixtures create the
    // usage_events table alone, so the update is gated on the journal table
    // actually existing.
    let journal_exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type = 'table' AND name = 'journal'",
        [],
        |row| row.get(0),
    )?;
    if journal_exists == 0 {
        return Ok(());
    }
    conn.execute(
        "UPDATE usage_events
         SET scope_id = (
             SELECT json_extract(j.payload_json, '$.payload.scopeId')
             FROM journal j
             WHERE j.instance_id = usage_events.instance_id
               AND j.seq = usage_events.seq
               AND json_valid(j.payload_json)
               AND json_extract(j.payload_json, '$.kind') = 'usage'
         )
         WHERE scope_id IS NULL
           AND EXISTS (
               SELECT 1 FROM journal j2
               WHERE j2.instance_id = usage_events.instance_id
                 AND j2.seq = usage_events.seq
                 AND json_valid(j2.payload_json)
                 AND json_extract(j2.payload_json, '$.kind') = 'usage'
                 AND json_extract(j2.payload_json, '$.payload.scopeId') IS NOT NULL
           )",
        [],
    )?;
    Ok(())
}

/// Recover the projection fields origin/main's — and the rolled-back
/// migration-window's — insert path did NOT persist, from the durable journal
/// (c-usagefu r16 item 8).
///
/// While a usage migration stayed rolled back, [`insert_legacy_row`] recorded
/// every usage event (codex Turn AND the new Message/Session frames) with only
/// the fourteen legacy columns: the additive columns the migration adds were
/// absent at write time and default to NULL/`'ingest'`/revision 1 afterward.
/// The scope-id backfill above only recovered `scope_id`; without the native
/// turn id the rollup's `COUNT(DISTINCT turn_id)` dropped every fallback
/// Message call, and the `'ingest'` source excluded them from the native TPM
/// windows and the eligible-flow relation — so fallback-period usage vanished
/// from totals, rates and attribution.
///
/// This re-projects the missing fields from the matching journal row
/// `(instance_id, seq)` the same way [`project_usage_event`] reads the live
/// event, filling ONLY missing/authoritative-default values (never overwriting
/// a value a native projection already supplied), and runs BEFORE
/// [`clear_disagreeing_recovered_turn_keys`] /
/// [`collapse_duplicate_turn_lineage_rows`] /
/// [`consolidate_message_duplicates`], so the fold winner and the
/// consolidation COALESCE actually have native siblings to merge.
/// Idempotent: every assignment is COALESCE/guarded.
fn reconstruct_fallback_projection_from_journal(conn: &Connection) -> rusqlite::Result<()> {
    // Same existence gate as the scope-id backfill (unit fixtures have no
    // journal table).
    let journal_exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type = 'table' AND name = 'journal'",
        [],
        |row| row.get(0),
    )?;
    if journal_exists == 0 {
        return Ok(());
    }
    // The correlated subqueries return NULL unless a valid kind='usage'
    // journal row carries the field; bare U64 scalars arrive as decimal
    // strings and are CAST to integers (guarded > 0 so garbage never zeroes a
    // stored positive value).
    conn.execute(
        "UPDATE usage_events
         SET turn_id = COALESCE(turn_id, (
                 SELECT json_extract(j.payload_json, '$.source.nativeTurnId.value')
                 FROM journal j
                 WHERE j.instance_id = usage_events.instance_id
                   AND j.seq = usage_events.seq
                   AND json_valid(j.payload_json)
                   AND json_extract(j.payload_json, '$.kind') = 'usage'
                   AND json_extract(j.payload_json, '$.source.nativeTurnId.value') IS NOT NULL
             )),
             native_context_window = COALESCE(native_context_window, (
                 SELECT CAST(json_extract(j.payload_json, '$.payload.contextWindow') AS INTEGER)
                 FROM journal j
                 WHERE j.instance_id = usage_events.instance_id
                   AND j.seq = usage_events.seq
                   AND json_valid(j.payload_json)
                   AND json_extract(j.payload_json, '$.kind') = 'usage'
                   AND CAST(json_extract(j.payload_json, '$.payload.contextWindow') AS INTEGER) > 0
             )),
             metric_revision = CASE
                 -- r38 item 1: raise ONLY. A durable Message keeps its original
                 -- stored sequence across revisions while the journal record at
                 -- that sequence carries the revision-1 frame, so re-projecting
                 -- must never lower an established revision watermark; a stale
                 -- replay would otherwise satisfy the >= revision accounting
                 -- checks. Fallback rows default to revision 1 and still get
                 -- their true (higher) journal revision back.
                 WHEN CAST((
                     SELECT json_extract(j.payload_json, '$.payload.metricRevision')
                     FROM journal j
                     WHERE j.instance_id = usage_events.instance_id
                       AND j.seq = usage_events.seq
                       AND json_valid(j.payload_json)
                       AND json_extract(j.payload_json, '$.kind') = 'usage'
                 ) AS INTEGER) > metric_revision
                 THEN CAST((
                     SELECT json_extract(j2.payload_json, '$.payload.metricRevision')
                     FROM journal j2
                     WHERE j2.instance_id = usage_events.instance_id
                       AND j2.seq = usage_events.seq
                       AND json_valid(j2.payload_json)
                       AND json_extract(j2.payload_json, '$.kind') = 'usage'
                 ) AS INTEGER)
                 ELSE metric_revision
             END,
             observed_at = CASE
                 WHEN observed_at_source = 'native' THEN observed_at
                 ELSE COALESCE((
                     SELECT json_extract(j.payload_json, '$.nativeAt.value')
                     FROM journal j
                     WHERE j.instance_id = usage_events.instance_id
                       AND j.seq = usage_events.seq
                       AND json_valid(j.payload_json)
                       AND json_extract(j.payload_json, '$.kind') = 'usage'
                       AND json_extract(j.payload_json, '$.nativeAt.value') IS NOT NULL
                 ), observed_at)
             END,
             observed_at_source = CASE
                 WHEN observed_at_source = 'native' THEN 'native'
                 WHEN EXISTS (
                     SELECT 1 FROM journal j
                     WHERE j.instance_id = usage_events.instance_id
                       AND j.seq = usage_events.seq
                       AND json_valid(j.payload_json)
                       AND json_extract(j.payload_json, '$.kind') = 'usage'
                       AND json_extract(j.payload_json, '$.nativeAt.value') IS NOT NULL
                 ) THEN 'native'
                 ELSE observed_at_source
             END
         WHERE EXISTS (
             SELECT 1 FROM journal j
             WHERE j.instance_id = usage_events.instance_id
               AND j.seq = usage_events.seq
               AND json_valid(j.payload_json)
               AND json_extract(j.payload_json, '$.kind') = 'usage'
         )",
        [],
    )?;
    Ok(())
}

/// Collapse pre-r9 duplicate Message rows (same instance_id, scope,
/// scope_id) into a single survivor each, then create the dedupe indexes.
///
/// Runs inside [`migrate`]'s single transaction (c-usagefu r14 item 1,
/// Ruling M1): it opens no transaction of its own and a failure anywhere
/// (including the unique-index creates) rolls the WHOLE migration back.
///
/// The survivor is the FIRST write (lowest rowid, preserving ingest
/// ordering). Ruling C1 (c-usagefu r10 item 3; r11 item 8), made EXACT by
/// c-usagefu r12 item 5 (Ruling C1'):
/// - the highest metric_revision is determined FIRST. Counters/cost
///   (total/input/output/cache buckets, cost_usd) AND the `accounting`
///   authority come ONLY from rows AT that revision; among same-revision
///   rows, the newest rowid reporting the bucket wins. A rev-1 row reporting
///   input 7 / cost 0.10 / estimated and a rev-2 row reporting NULL input /
///   NULL cost / reported leave the survivor at rev 2 with input and cost
///   STILL NULL — the older revision's known values never fill the newer
///   revision's gaps (the first-write 10/0.01/estimated never returns). A
///   NULL bucket is repaired ONLY from another non-NULL row at the SAME
///   highest revision;
/// - revision: the maximum;
/// - profile/model/turn id/native context window/observed time are
///   ATTRIBUTION and merge best-known (any non-NULL sibling, at ANY
///   revision; native time over ingest time), independent of which revision
///   owned the accounting.
///
/// A second run changes nothing: one row per key makes the UPDATE/DELETE
/// no-ops and the index creates are `IF NOT EXISTS`.
fn consolidate_message_duplicates(conn: &Connection) -> rusqlite::Result<()> {
    // C1' (r12 item 5): the accounting assignments are generated from one
    // shared "highest revision first" fragment. The top revision is resolved
    // FIRST; every counter/cost column and the accounting authority come only
    // from rows AT that revision (newest rowid among them), and a NULL is
    // repaired only by a non-NULL sibling at the SAME revision — an older
    // revision's known value never fills it.
    let top_revision = "(
                 SELECT MAX(m3.metric_revision) FROM usage_events m3
                 WHERE m3.instance_id = usage_events.instance_id
                   AND m3.scope = 'message' AND m3.scope_id = usage_events.scope_id)";
    let mut accounting_sets = String::new();
    for column in [
        "total_tokens",
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "cost_usd",
    ] {
        accounting_sets.push_str(&format!(
            ",{column} = (
                 SELECT m2.{column} FROM usage_events m2
                 WHERE m2.instance_id = usage_events.instance_id
                   AND m2.scope = 'message' AND m2.scope_id = usage_events.scope_id
                   AND m2.metric_revision = {top_revision}
                   AND m2.{column} IS NOT NULL
                 ORDER BY m2.rowid DESC LIMIT 1)"
        ));
    }
    // accounting is NOT NULL on every row, so no IS NOT NULL filter: the
    // newest row at the top revision is the authority.
    accounting_sets.push_str(&format!(
        ",accounting = (
                 SELECT m2.accounting FROM usage_events m2
                 WHERE m2.instance_id = usage_events.instance_id
                   AND m2.scope = 'message' AND m2.scope_id = usage_events.scope_id
                   AND m2.metric_revision = {top_revision}
                 ORDER BY m2.rowid DESC LIMIT 1)"
    ));
    let batch = format!(
        "UPDATE usage_events
         SET profile_id = COALESCE(profile_id,
             (SELECT m2.profile_id FROM usage_events m2
              WHERE m2.instance_id = usage_events.instance_id
                AND m2.scope = 'message' AND m2.scope_id = usage_events.scope_id
                AND m2.profile_id IS NOT NULL LIMIT 1)),
             model = COALESCE(model,
             (SELECT m2.model FROM usage_events m2
              WHERE m2.instance_id = usage_events.instance_id
                AND m2.scope = 'message' AND m2.scope_id = usage_events.scope_id
                AND m2.model IS NOT NULL LIMIT 1)),
             turn_id = COALESCE(turn_id,
             (SELECT m2.turn_id FROM usage_events m2
              WHERE m2.instance_id = usage_events.instance_id
                AND m2.scope = 'message' AND m2.scope_id = usage_events.scope_id
                AND m2.turn_id IS NOT NULL LIMIT 1)),
             metric_revision = {top_revision}
             {accounting_sets},
             native_context_window = COALESCE(native_context_window,
             (SELECT m2.native_context_window FROM usage_events m2
              WHERE m2.instance_id = usage_events.instance_id
                AND m2.scope = 'message' AND m2.scope_id = usage_events.scope_id
                AND m2.native_context_window IS NOT NULL LIMIT 1)),
             observed_at = CASE
                 WHEN observed_at_source = 'native' THEN observed_at
                 WHEN EXISTS (
                     SELECT 1 FROM usage_events m2
                     WHERE m2.instance_id = usage_events.instance_id
                       AND m2.scope = 'message' AND m2.scope_id = usage_events.scope_id
                       AND m2.observed_at_source = 'native')
                 THEN (
                     SELECT m2.observed_at FROM usage_events m2
                     WHERE m2.instance_id = usage_events.instance_id
                       AND m2.scope = 'message' AND m2.scope_id = usage_events.scope_id
                       AND m2.observed_at_source = 'native'
                     LIMIT 1)
                 ELSE observed_at END,
             observed_at_source = CASE
                 WHEN observed_at_source = 'native' THEN 'native'
                 WHEN EXISTS (
                     SELECT 1 FROM usage_events m2
                     WHERE m2.instance_id = usage_events.instance_id
                       AND m2.scope = 'message' AND m2.scope_id = usage_events.scope_id
                       AND m2.observed_at_source = 'native')
                 THEN 'native'
                 ELSE observed_at_source END
         WHERE scope = 'message' AND scope_id IS NOT NULL
           AND rowid IN (
               SELECT MIN(rowid) FROM usage_events
               WHERE scope = 'message' AND scope_id IS NOT NULL
               GROUP BY instance_id, scope, scope_id)
           AND EXISTS (
               SELECT 1 FROM usage_events m3
               WHERE m3.instance_id = usage_events.instance_id
                 AND m3.scope = 'message' AND m3.scope_id = usage_events.scope_id
                 AND m3.rowid != usage_events.rowid);

         DELETE FROM usage_events
         WHERE scope = 'message' AND scope_id IS NOT NULL
           AND rowid NOT IN (
               SELECT MIN(rowid) FROM usage_events
               WHERE scope = 'message' AND scope_id IS NOT NULL
               GROUP BY instance_id, scope, scope_id);

         DROP INDEX IF EXISTS usage_events_scope_dedupe;
         -- The TURN dedupe index is intentionally NOT recreated here: the
         -- lineage-aware index (instance_id, scope, scope_id, COALESCE(
         -- source_session_id, '')) is created at the end of the migration;
         -- main's 3-column index would win via IF NOT EXISTS and forbid two
         -- proven lineages that legitimately share a scope id
         -- (c-ctxusage r16 item 3 / T1').
         -- c-usagefu r7 item 5: codex Message rows are keyed on the durable
         -- per-response id; re-promoting/replaying a rollout byte-0 re-emits
         -- the same response and must not double the Message totals.
         CREATE UNIQUE INDEX IF NOT EXISTS usage_events_message_dedupe
             ON usage_events(instance_id, scope, scope_id)
             WHERE scope = 'message' AND scope_id IS NOT NULL;"
    );
    conn.execute_batch(&batch)?;
    Ok(())
}

/// Inserts a usage projection.
///
/// Dedupe semantics by whether the row carries a durable `scope_id`:
/// - Rows with no `scope_id` are append-only: plain `INSERT OR IGNORE` on
///   `(instance_id, seq)`.
/// - **Turn** rows (a Claude assistant message id) are revision-aware: the
///   provisional stream record is replaced in place by the final one
///   ([`upsert_turn_snapshot`]).
/// - **Session** rows (cumulative Grok/Codex stock keyed by the stable session
///   id) are append-only GROWTH POINTS ([`insert_session_growth_point`]): a
///   restarted adapter replays historical stocks with fresh seqs, so acceptance
///   is decided by the cumulative counters themselves (never smaller than the
///   current stock, never an identical stock twice), not by ingestion order.
///
/// Turn rows are therefore NOT keep-first: a message that streamed a stop record
/// at one poll and then a later same-message-id record with different counters
/// (c-ctxusage r3 item 1) replaces the provisional row with the final one.
pub fn insert_usage_event(conn: &Connection, row: &UsageEventRow) -> rusqlite::Result<bool> {
    // c-usagefu r15 item 1: on a database whose usage migration never
    // committed (the legacy origin/main shape), record the event through
    // main's plain insert path so usage keeps landing durably — a later
    // successful migration's journal backfill recovers the scope ids. The
    // scoped upsert paths reference columns the legacy table does not have.
    if !usage_schema_is_current(conn)? {
        return insert_legacy_row(conn, row);
    }
    match (row.scope.as_str(), row.scope_id.as_deref()) {
        ("turn", Some(_)) => upsert_turn_snapshot(conn, row),
        ("session", Some(_)) => insert_session_growth_point(conn, row),
        ("session", None) => insert_legacy_session_point(conn, row),
        (_, None) => insert_row(conn, row),
        _ => insert_row(conn, row),
    }
}

/// origin/main's projection insert: 14 columns, append-only `INSERT OR
/// IGNORE` on `(instance_id, seq)`. Used ONLY while the database is still on
/// the legacy shape (a rolled-back migration); the journal retains the full
/// event, so the post-migration backfill loses nothing.
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

/// Query every cumulative session stock a new scoped point must not move
/// backwards against. Floors apply ONLY within a proven matching session id:
/// the point's OWN `scope_id` chain (c-ctxusage r8 item 8).
///
/// A legacy NULL-scope session row carries no session identity at all, so it
/// can never be proven to belong to the incoming scoped point's session. The
/// old "floor the first scoped row with the legacy total" rule froze a
/// DIFFERENT, smaller first post-upgrade session's genuine usage; it was
/// removed. Legacy rows still floor other legacy rows in
/// [`insert_legacy_session_point`].
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

/// One cumulative session stock: token buckets plus the reported cumulative
/// cost. Both are cumulative monotonic quantities, so both participate in the
/// growth/freeze decision (c-usagefu r3 item 3: the result frame carries no
/// tokens, so a token-only comparison froze every later result at the turn-1
/// cumulative cost).
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

    /// Equality of every reported bucket INCLUDING cost.
    fn eq_stock(&self, other: &Self) -> bool {
        self.counters == other.counters && self.cost == other.cost
    }

    /// Whether a reported TOKEN bucket grew against the stored stock.
    fn tokens_grew_against(&self, stored: &Self) -> bool {
        self.counters.growth_against(&stored.counters).0
    }

    /// A smaller reported COST is a freeze only on a token-LESS point: that is
    /// the result frame's cumulative cost channel, whose currency is
    /// monotonic. On a point whose token counters grew, a lower cost is a
    /// driver-priced estimate reset (an unpriced model turn followed by a
    /// priced one restarted the running total); freezing it would stall the
    /// context chip until the running total catches up (c-usagefu r4 item 4).
    /// The stale cost is dropped on insert instead (see
    /// [`with_reset_cost_stripped`]).
    fn cost_decrease_blocks(&self, stored: &Self) -> bool {
        let cost_went_down = matches!(
            (self.cost, stored.cost),
            (Some(incoming), Some(previous)) if incoming < previous
        );
        cost_went_down && !self.tokens_grew_against(stored)
    }
}

/// Parse a stored decimal cost; a malformed value behaves as "not reported".
fn parse_cost(cost: Option<String>) -> Option<f64> {
    cost.and_then(|raw| raw.parse::<f64>().ok())
}

/// Content-ordered acceptance against a set of existing stocks.
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

/// When a token-growing point carries a lower cost than the stored stock, the
/// driver's running estimate has reset (unpriced turn → priced turn). Drop
/// that point's cost rather than persist a smaller cumulative value; its
/// token counters remain the real content. Token-LESS points keep their cost
/// (the result channel is monotonic and is frozen on a decrease).
fn with_reset_cost_stripped(
    mut row: UsageEventRow,
    incoming: &SessionStockPoint,
    floors: &[SessionStockPoint],
) -> UsageEventRow {
    let tokens_grew = floors
        .iter()
        .any(|stored| incoming.tokens_grew_against(stored));
    let cost_reset = floors
        .iter()
        .any(|stored| matches!((incoming.cost, stored.cost), (Some(new), Some(old)) if new < old));
    if tokens_grew && cost_reset {
        row.cost_usd = None;
    }
    row
}

/// Append a new scoped cumulative-session growth point, freezing
/// historical/duplicate replays (c-ctxusage r4 item 3, refined in r8 item 8).
///
/// The cumulative stock is monotonic by contract, so acceptance is content-
/// based and independent of the restart-unsafe producer revision and ingest
/// seq:
/// - identical counters AND cost already recorded for this session (or for a
///   legacy NULL-scope row) -> frozen;
/// - any reported TOKEN bucket smaller than the current stock -> frozen;
/// - a smaller cumulative COST on a token-LESS point (the result channel)
///   -> frozen; the same lower cost on a token-growing point is a driver
///   estimate reset: the point appends with its cost dropped (r4 item 4);
/// - otherwise the point is appended.
///
/// Legacy NULL-scope rows carry no session identity and therefore never floor
/// a scoped point (r8 item 8).
fn insert_session_growth_point(conn: &Connection, row: &UsageEventRow) -> rusqlite::Result<bool> {
    let incoming = SessionStockPoint::of(row);
    let floors = existing_session_floors(conn, row)?;
    if freezes_against(&incoming, &floors) {
        return Ok(false);
    }
    let row = with_reset_cost_stripped(row.clone(), &incoming, &floors);
    insert_row(conn, &row)
}

/// Legacy (pre-`scopeId`) session row: content-ordered against other legacy
/// rows only. Scoped points are the newer world and are summed separately in
/// the rollup; legacy totals are read at all only while no scoped rows exist.
fn insert_legacy_session_point(conn: &Connection, row: &UsageEventRow) -> rusqlite::Result<bool> {
    let incoming = SessionStockPoint::of(row);
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
    if freezes_against(&incoming, &existing) {
        return Ok(false);
    }
    let row = with_reset_cost_stripped(row.clone(), &incoming, &existing);
    insert_row(conn, &row)
}

/// Generic append (`INSERT OR IGNORE` on the `(instance_id, seq)` PK).
/// Message rows take the upsert path (c-usagefu r8 item 5): a replayed
/// response with the SAME durable (instance, scope, scope_id) updates the
/// stored row when it carries better attribution (a turn_id / native
/// timestamp the first write lacked) rather than being silently dropped.
fn insert_row(conn: &Connection, row: &UsageEventRow) -> rusqlite::Result<bool> {
    if row.scope == "message" && row.scope_id.is_some() {
        return upsert_message_row(conn, row);
    }
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO usage_events
            (instance_id, seq, profile_id, model, scope, scope_id, turn_id, mode,
             metric_revision, source_session_id, total_tokens, input_tokens, output_tokens,
             cache_read_tokens, cache_write_tokens, cost_usd, accounting,
             observed_at, observed_at_source, native_context_window)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
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
    )?;
    Ok(inserted > 0)
}

/// Insert or improve a durable Message row keyed on
/// (instance_id, scope='message', scope_id).
///
/// c-usagefu r11 item 9 (Ruling C1, EXACT) — live duplicate updates are
/// REVISION-AUTHORITATIVE for accounting:
/// - a newer revision (`metric_revision`) replaces present counters/cost and
///   the `accounting` authority outright, and raises the stored revision; its
///   NULL buckets impose nothing;
/// - an OLDER revision never replaces anything; its known buckets do not even
///   fill NULLs — a NULL->known repair is allowed only at the SAME or a NEWER
///   revision;
/// - attribution (turn_id, native timestamp/source) still repairs best-known
///   independent of revision.
///
/// Returns true when a row was inserted OR at least one value changed.
fn upsert_message_row(conn: &Connection, row: &UsageEventRow) -> rusqlite::Result<bool> {
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO usage_events
            (instance_id, seq, profile_id, model, scope, scope_id, turn_id, mode,
             metric_revision, total_tokens, input_tokens, output_tokens,
             cache_read_tokens, cache_write_tokens, cost_usd, accounting,
             observed_at, observed_at_source, native_context_window)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                 ?15, ?16, ?17, ?18, ?19)",
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
                 ELSE ?11 END
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
        ],
    )?;
    Ok(updated > 0)
}

/// Counters carried by one scoped snapshot, in fold order.
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

    /// The four independent token baskets, excluding the derived total.
    fn token_baskets(&self) -> [Option<i64>; 4] {
        [self.input, self.output, self.cache_read, self.cache_write]
    }

    /// Per-bucket comparison of an incoming cumulative snapshot against the
    /// stored one. `None` incoming means "not reported" and imposes no
    /// constraint; `None` stored with a reported incoming is growth.
    ///
    /// Returns `(grew, decreased)`: at least one reported bucket advanced, and
    /// no reported bucket moved backwards.
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

    /// Merge a PARTIAL incoming snapshot over the retained counters for the
    /// scoped UPDATE. An unreported bucket (`None`) means "not reported", not
    /// "zero": two print result frames for the same scope where the later one
    /// carries only `output_tokens` must not erase the known input/cache
    /// counters (c-ctxusage r12 item 2). A reported total stays authoritative;
    /// otherwise the total is derived from the merged basket
    /// (`input + output + cache_read + cache_write`) ONLY WHEN every basket is
    /// Known — a single unknown constituent leaves the total Unknown, never a
    /// partial sum (U1, c-ctxusage r16 item 4).
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

/// Turn authority gate (c-ctxusage r16 item 3, T1'): is `position` equal or
/// older than THIS lineage's stored watermark?
///
/// The watermark is kept PER LINEAGE `(scope_id, source_session_id)` — the
/// caller looks up ONLY the row belonging to the incoming frame's lineage
/// (NULL is the unqualified legacy lineage, a distinct key from every
/// qualified session id), so the comparison is always within one proven
/// lineage: another lineage's acceptance never erases the mark and a frame
/// from a different lineage reaches the insert branch with a row of its own.
fn turn_position_is_stale(incoming_position: i64, stored_revision: i64) -> bool {
    incoming_position <= stored_revision
}

/// Insert or revise a Turn-scoped snapshot.
///
/// One REVISION-REPLACED ROW PER LINEAGE (c-ctxusage r16 item 3): the
/// authority watermark is keyed by `(scope_id, source_session_id)`, with NULL
/// (the unqualified legacy lineage) as its own lineage.
///
/// Turn authority — replay-stable source positions (c-ctxusage r14 items 2-4):
/// `metric_revision` carries the source's REPLAY-STABLE position of the record
/// that produced the snapshot (a session-qualified transcript record index),
/// never a per-mapper publication counter. The Hub treats it as an authority
/// WATERMARK per `(instance, turn scope, scope_id, lineage)`:
///
/// * a snapshot at a STRICTLY NEWER position within its OWN lineage is applied
///   wholesale — every bucket it explicitly reports replaces the stored one,
///   a smaller value allowed; buckets it does not report are retained; the
///   derived total is recomputed; a reported cost replaces the stored bill
///   while an unreported cost keeps it; the turn clock moves with the newer
///   snapshot (an ingest fallback still never overwrites a native time). The
///   watermark is persisted EVEN when every value is unchanged;
/// * an EQUAL- or OLDER-position snapshot changes NOTHING — no counters, no
///   equal-position growth or bucket fill, no cost, no clock
///   (c-ctxusage r15 item 2). The gate runs BEFORE the cost-only branch, so a
///   billing correction at a stale position is rejected too;
/// * a STRICTLY NEWER COST-ONLY frame (no token baskets reported) is a
///   billing correction on a separate channel: the bill alone is corrected
///   (an unreported bill keeps the stored one) and the watermark advances, so
///   a later counter snapshot at an older position is then rejected;
///   counters it did not come with and the turn clock are kept
///   (c-ctxusage r15 item 3);
/// * a frame from a DIFFERENT lineage finds no row of its own and is inserted
///   as a separate revision-replaced row: accepting it never erases another
///   lineage's mark, and an unqualified frame never rewrites a qualified
///   row's identity (no COALESCE onto a retained qualified id). Read models
///   fold one winning row per scope id (c-ctxusage r16 item 3).
///
/// Cumulative SESSION snapshots have their own monotone acceptance path and
/// are not governed here.
fn upsert_turn_snapshot(conn: &Connection, row: &UsageEventRow) -> rusqlite::Result<bool> {
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
            })
        };
        const STORED_SELECT: &str = "SELECT rowid, seq, profile_id, model, mode,
                    metric_revision, source_session_id,
                    total_tokens, input_tokens, output_tokens,
                    cache_read_tokens, cache_write_tokens,
                    cost_usd, accounting, observed_at, observed_at_source
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
        // every other lineage's row and watermark untouched.
        return insert_row(conn, row);
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

/// Decode an additive token bucket (`Knowledge<U64>`) the wire marks
/// `not-applicable` as a STRUCTURAL ZERO rather than Unknown.
///
/// A model without a cache tier (or a frame that cannot produce a bucket)
/// sends `{"state":"not-applicable"}`: that is a definite "this contributes
/// 0", not "the value was not reported". Main's context-basket sum treated
/// such a column as zero; the unified fold instead demanded a value in every
/// bucket and dropped the whole context to Unknown whenever cache creation
/// was N/A (the supply r2c shape), regressing the context chip (r37 gate
/// triage: passes on cbac58e1, fails on the merge). An OMITTED key or an
/// explicit `unknown` stays None — U1 still hides a genuinely unreported
/// bucket.
fn additive_knowledge_u64(value: Option<&Value>) -> Option<i64> {
    let payload = value?;
    if payload.get("state").and_then(Value::as_str) == Some("not-applicable") {
        return Some(0);
    }
    knowledge_u64(payload)
}

/// Decode a bare protocol `U64` scalar — a canonical decimal string (`"2"`),
/// tolerating a bare JSON number from non-Rust producers — as an `i64`.
/// Distinct from [`knowledge_u64`], which unwraps a `Knowledge<U64>` enum's
/// `{state,value}` object (c-ctxusage r3 item 2).
/// Decode a bare JSON number or decimal string as an `i64` (a protocol
/// `U64` payload may arrive either way).
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
        source_session_id: payload
            .get("sourceSessionId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned),
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

/// Aggregated usage for one `(profile, model)` or instance.
///
/// Every counter and the bill are Known only when EVERY folded contributing
/// row reported/priced them: one omitted bucket or unpriced turn makes the
/// aggregate Unknown (`None`) rather than a partial sum (U1,
/// c-ctxusage r16 item 4).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageAggregate {
    /// Proven contributing rows folded (latest winning stocks + proven
    /// uncovered turns + other-scope events). Ambiguous turns are NOT rows
    /// here — they cannot be summed — but see [`Self::ambiguous`].
    pub events: i64,
    /// Rows that exist in retained data but cannot be proven to belong to a
    /// native session (NULL lineage) in an instance that HAS scoped stock.
    /// They contribute no numeric value (U1: the fold cannot certify their
    /// buckets), but they must never be silently dropped from reporting: a
    /// provider model whose only usage is such a turn still gets a row with
    /// Unknown totals/budget (c-ctxusage r34 item 1).
    pub ambiguous_events: i64,
    /// Whether any ambiguous turn exists anywhere in this fold's instances —
    /// used by the per-instance totals, which sum across models.
    pub ambiguous: bool,
    /// Summed total tokens; Unknown when any contributing row omitted it
    /// (U1), or on the legacy pre-migration shape (see `unavailable`).
    pub total_tokens: Option<i64>,
    /// Summed input tokens; Unknown when any contributing row omitted it.
    pub input_tokens: Option<i64>,
    /// Summed output tokens; Unknown when any contributing row omitted it.
    pub output_tokens: Option<i64>,
    /// Summed cost (decimal, estimated). `None` when ANY folded contributing
    /// row is unpriced (U1), or on the legacy pre-migration shape.
    pub cost_usd: Option<f64>,
    /// c-usagefu r16 item 7: true on the rolled-back/legacy shape — the numeric
    /// fields are NOT evidence (placeholders, not real spend). Distinct from a
    /// real zero (a migrated instance with no spend yet), which leaves this
    /// false.
    pub unavailable: bool,
}

impl UsageAggregate {
    /// The explicit Unavailable state for a store the usage migration did not
    /// upgrade (c-usagefu r16 item 7).
    pub(crate) fn unavailable() -> Self {
        Self {
            unavailable: true,
            ..Self::default()
        }
    }

    /// Budget status against a cap. `Unknown` when the bill itself is Unknown
    /// — an unpriced contributing turn (U1, c-ctxusage r16 item 4) OR
    /// unprovable ambiguous-lineage usage (c-ctxusage r34 item 1); an unknown
    /// bill can never be certified under budget. On the legacy shape the
    /// provider surface reports "ok" with the Unavailable flag rather than
    /// grading a band off the placeholder (c-usagefu r16 item 7).
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

/// Budget reconciliation state; §4.5.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BudgetStatus {
    /// The known estimated bill is under the cap.
    Ok,
    /// At the estimate (warn).
    Warn,
    /// At estimate × 1.15 (stop band).
    Stop,
    /// The bill is Unknown (an unpriced contributing turn): the cap can
    /// neither be confirmed satisfied nor breached (U1, c-ctxusage r16 item 4).
    Unknown,
}

/// c-usagefu r13 COORDINATOR RULING U1: ONE shared eligible-FLOW relation by
/// CALL identity, used by the rollup, both aggregates and the TPM windows.
///
/// U1 bounds the upgrade: usagefu never landed, so the only databases in the
/// field are (a) those written by origin/main's schema and codex insert path
/// (turn- and session-scope rows only, NO codex Message rows) and (b) those
/// written by the final schema (every Message carries the durable
/// (rollout file, byte offset) identity and its turn id). Shapes produced only
/// by intermediate unlanded rounds are out of scope and need no reconciliation.
///
/// Each native turn is counted EITHER from its Turn row OR from its Message
/// rows, never both:
///
/// * A linked Message (`turn_id` present) is counted when either:
///   * its turn has NO Turn row — a final-shape turn without a Turn row counts
///     its durable linked Messages (a zero-token record contributes nothing);
///     or
///   * the Turn row EXISTS and the durable linked Messages match it on every
///     token bucket it reports ([`turn_fully_covered_sql`]) — bucket-sum
///     equality IS the completeness proof. A mismatch counts the Turn and drops
///     the linked Messages.
/// * A Message WITHOUT `turn_id` is the pre-r6 attribution-less Message: B1
///   needs no clocks — when the instance has ANY Turn row at all every such
///   Message is dropped; an instance with no Turn rows whatsoever counts all
///   of them.
/// * A Turn row is counted only when its durable linked Messages do NOT match
///   it bucket-for-bucket. A main-shape Turn row (which main projected without
///   a scope_id) has its scope_id backfilled from the journal at migration, so
///   a post-upgrade byte-0 replay's durable Messages pair with it and suppress
///   it once the sums match.
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

/// SQL boolean: at least one linked Message of `(inst, tid)` carries the
/// persisted durable identity marker the final codex adapter mints for every
/// counted record. `alias` must be unique per use inside one enclosing
/// statement (SQLite forbids duplicate correlation aliases).
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

/// True when the DURABLE linked Messages of turn `(inst, tid)` match its
/// WINNING Turn row on every token bucket the winning row actually reports
/// (COORDINATOR RULING U1, c-usagefu r13 item 2; r37 item 5).
///
/// The comparison row is the fold winner — the same `seq DESC, rowid DESC`
/// ordering `global_winning_turns` uses across ALL native-session lineages —
/// never an arbitrary raw row: per-lineage retention can leave several Turn
/// rows for one scope id, and matching the Messages against the losing
/// lineage's smaller buckets would suppress the real winner.
///
/// Bucket-sum equality between the winning Turn row and its durable linked
/// Messages IS the completeness proof:
///
/// - at least one linked Message carries the persisted durable (rollout file
///   identity, byte offset) identity the final adapter mints for EVERY
///   token_usage_record, and every bucket SUM runs over DURABLE linked
///   Messages only. Main wrote no codex Message rows at all and the final
///   schema writes none without the durable marker, so no per-record identity
///   mix can occur in an in-scope database;
/// - for every TOKEN bucket the winning Turn row reports, the durable
///   Messages' sum EQUALS it. A bucket the winner leaves NULL (a sparse
///   historical Turn never carried total/cache-write) cannot disprove
///   coverage and is not compared; the Messages may additionally report
///   buckets the Turn omitted. A zero-token record contributes nothing (it
///   adds zero to the sums), so it neither creates nor breaks equality;
/// - when the durable sums fall short of the winner on any reported bucket
///   the turn is NOT covered: the Turn is billed and its linked Messages are
///   dropped — never both.
///
/// Cost is deliberately NOT part of the proof: it is a priced projection that
/// may legitimately differ from a historical Turn.
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

/// The unified usage fold shared by the instance/provider aggregates, the
/// session rollup and the TPM rate windows (c-ctxusage × c-usagefu merge).
///
/// It reconciles two independently-reviewed billing models:
///
/// * **Lineage fold (c-ctxusage):** turn rows fold to ONE winner per
///   `(instance_id, scope_id)` across native-session lineages; a cumulative
///   SESSION stock of session `S` covers only a turn whose provenance stamps
///   `source_session_id = S`; an unprovable NATIVE-time turn beside scoped
///   stock is *ambiguous* — dropped from the fold and poisoned to Unknown,
///   never guessed.
/// * **Flow vs stock, durable Messages (c-usagefu Ruling B / r7-r10):**
///   codex emits per-call durable Message rows; a Turn a matching Message
///   fully covers is dropped (bucket-sum coverage proof, [`turn_fully_covered_sql`]);
///   an INGEST-time unqualified turn is live per-call flow — when any such
///   flow exists it bills and every session stock of the instance stands down
///   (the sticky-Unknown / freeze fix); stock is summed per session ONLY for
///   flow-less sessions (c-ctxusage r28 per-session composition).
///
/// The native/ingest distinction on unqualified turns is what the two rulings
/// disagree on: an UNPROVEN backfilled NATIVE turn next to stock stays Unknown
/// (the conservative c-ctxusage answer), while a live INGEST-provenance flow
/// row bills per Ruling B. The final codex adapter emits no live turn rows at
/// all, so the two shapes never meet in production.
///
/// `domain_pred` filters the final CTEs: `"instance_id = ?1"` for the instance
/// aggregate, `"profile_id = ?1 AND model = ?2"` for provider supply. All
/// bindings reuse SQLite NUMBERED positional parameters, so one/two bindings
/// cover every occurrence.
/// The fold graph is a ~41 KB static SQL body parameterized only by a fixed
/// domain predicate (exactly two shapes are ever used). Preparing it costs
/// ~115 ms in SQLite's planner, so both the text and its prepared statements
/// must be reused, never rebuilt/replanned per call. Memoize the text; the
/// caller runs it through `Connection::prepare_cached` on the long-lived store
/// connection so planning happens once per process per statement.
fn fold_ctes_instance() -> &'static str {
    static C: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    C.get_or_init(|| unified_fold_ctes("instance_id = ?1", "global_reported_session_cost"))
}

fn fold_ctes_supply() -> &'static str {
    static C: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        // Supply folds the newest bill per MODEL+session (r39 item 4).
        unified_fold_ctes(
            "profile_id = ?1 AND model = ?2",
            "global_reported_cost_by_model",
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
            SELECT * FROM (
                SELECT *, ROW_NUMBER() OVER (
                    PARTITION BY instance_id, scope_id ORDER BY seq DESC, rowid DESC
                ) AS stock_rn
                FROM usage_events
                WHERE scope = 'session' AND scope_id IS NOT NULL
            )
            WHERE stock_rn = 1
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
            -- r39 item 4 (AA3 provider attribution): the SAME session can
            -- report cumulative snapshots against different models across a
            -- /model switch. The instance fold (global_reported_session_cost)
            -- takes the newest per session; the PROVIDER fold needs the newest
            -- snapshot per (model, session) so an earlier model keeps its
            -- reported bill instead of being hidden by the switched model.
            -- Scoped only: a legacy NULL bill is instance-keyed and already in
            -- the main relation.
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
         global_legacy_stock AS (
            SELECT * FROM (
                SELECT *, ROW_NUMBER() OVER (
                    PARTITION BY instance_id ORDER BY rowid DESC
                ) AS legacy_rn
                FROM usage_events
                WHERE scope = 'session' AND scope_id IS NULL
            )
            WHERE legacy_rn = 1
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
            SELECT s.instance_id AS instance_id, s.scope_id AS sid
            FROM global_scoped_sessions s
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
            -- r39 (AA2): an instance with a native REPORTED cumulative bill is
            -- removed — reported-cost authority keeps the session carrier alive
            -- even on a result-only stream whose unqualified Turn set the
            -- Ruling-B flag (otherwise its own bill would be suppressed).
            SELECT instance_id FROM g_flow_messages
            UNION
            SELECT instance_id FROM g_unmatched_flow_turns
            UNION
            SELECT instance_id FROM g_legacy_open_turns
            UNION
            SELECT instance_id FROM g_legacy_flow_turns
            EXCEPT
            SELECT instance_id FROM global_reported_session_cost
         ),
         -- DOMAIN PROJECTION: the classification above is instance-global so a
         -- row stays correctly suppressed/ambiguous under every supply view;
         -- only the final projection filters by instance or profile/model.
         flow_messages AS (
            SELECT m.* FROM g_flow_messages m
            WHERE {dm}
              AND m.instance_id NOT IN (SELECT instance_id FROM reported_instances)
         ),
         flow_turns AS (
            -- AA2/AA3: flow Turns keep their TOKEN contribution even for
            -- instances with reported-cost authority (tokens and cost are
            -- independent); the cost CASE below excludes their cost. Ruling-B
            -- override/stock selection is handled by the global CTEs above.
            SELECT t.* FROM g_flow_turns t WHERE {dt}
         ),
         ambiguous_turns AS (
            SELECT t.* FROM g_ambiguous_turns t WHERE {dt}
         ),
         stock_sessions AS (
            -- Token-bearing session stock. A r39 native REPORTED carrier with
            -- real token buckets survives the Ruling-B flow override (reported
            -- cost authority); a token-LESS reported carrier is excluded here
            -- (its NULL buckets would poison U1) and contributes only via
            -- reported_session_cost / cost_only_reported.
            SELECT g.* FROM global_scoped_sessions g
            WHERE {dg}
              AND g.accounting = 'reported'
              AND (g.total_tokens IS NOT NULL OR g.input_tokens IS NOT NULL
                   OR g.output_tokens IS NOT NULL OR g.cache_read_tokens IS NOT NULL
                   OR g.cache_write_tokens IS NOT NULL)
            UNION ALL
            -- Flow-less sessions count their token stock unless a Ruling B
            -- instance-wide override (message/unqualified flow) stands it down.
            SELECT g.* FROM global_scoped_sessions g
            WHERE {dg}
              AND g.accounting <> 'reported'
              AND g.instance_id NOT IN (SELECT instance_id FROM override_instances)
              AND g.instance_id NOT IN (SELECT instance_id FROM reported_instances)
              AND NOT EXISTS (
                  SELECT 1 FROM g_session_turn_flow f
                  WHERE f.instance_id = g.instance_id AND f.sid = g.scope_id
              )
            UNION ALL
            -- Sessions with lineage-certified turn flow: the stock counts
            -- when it certifies the flow (equal/newer position), provided no
            -- instance-wide Ruling B override is active; a surviving uncovered
            -- proven turn of THIS session drops the stock via the missing
            -- coverage match below.
            SELECT g.* FROM global_scoped_sessions g
            WHERE {dg}
              AND g.accounting <> 'reported'
              AND g.instance_id NOT IN (SELECT instance_id FROM override_instances)
              AND g.instance_id NOT IN (SELECT instance_id FROM reported_instances)
              AND EXISTS (
                  SELECT 1 FROM g_fully_covered_sessions c
                  WHERE c.instance_id = g.instance_id AND c.sid = g.scope_id
              )
         ),
         legacy_stock AS (
            SELECT l.* FROM global_legacy_stock l
            WHERE {dl}
              AND l.instance_id NOT IN (
                  SELECT instance_id FROM global_scoped_sessions
              )
              AND l.instance_id NOT IN (
                  SELECT instance_id FROM override_instances
              )
              AND l.instance_id NOT IN (SELECT instance_id FROM reported_instances)
              AND NOT EXISTS (
                  SELECT 1 FROM g_flow_turns p WHERE p.instance_id = l.instance_id
              )
         ),
         reported_session_cost AS (
            -- Newest reported cumulative bill per session (instance fold) or
            -- per model+session (supply fold, r39 item 4), domain-projected.
            SELECT g.* FROM {reported_source} g WHERE {dg}
         ),
         reported_instances AS (
            -- r39 item 2 (AA2): instances in THIS domain with a reported bill.
            -- Their cost comes solely from reported_session_cost; their
            -- unqualified flow/stock estimates must not be added (nor, for a
            -- different instance, suppressed).
            SELECT DISTINCT instance_id FROM reported_session_cost
         ),
         -- r39 item 4 (AA3): a reported cost carrier whose SESSION frame
         -- itself reported no tokens (a cost-only result after a model switch)
         -- still gives the provider/model a cost row even when the Ruling-B
         -- flow override hides the token-less stock — token presence is
         -- independent of cost. Its token buckets are NULL so the U1 gates
         -- surface Unknown tokens.
         cost_only_reported AS (
            SELECT rc.total_tokens, rc.input_tokens, rc.output_tokens,
                   rc.cache_read_tokens, rc.cache_write_tokens, rc.cost_usd
            FROM reported_session_cost rc
            WHERE rc.total_tokens IS NULL
              AND rc.input_tokens IS NULL
              AND rc.output_tokens IS NULL
              AND rc.cache_read_tokens IS NULL
              AND rc.cache_write_tokens IS NULL
         ),
         contributions AS (
            SELECT total_tokens, input_tokens, output_tokens,
                   cache_read_tokens, cache_write_tokens, cost_usd
            FROM stock_sessions
            UNION ALL
            SELECT total_tokens, input_tokens, output_tokens,
                   cache_read_tokens, cache_write_tokens, cost_usd
            FROM legacy_stock
            UNION ALL
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
            FROM cost_only_reported
         )",
        dm = d("m"),
        dt = d("t"),
        dg = d("g"),
        dl = d("l"),
        reported_source = reported_source,
    )
}

/// Final aggregate SELECT over a [`unified_fold_ctes`] block. The U1
/// all-buckets-known gate applies to the TURN+STOCK relation (a turn
/// snapshot or cumulative point is one complete accounting frame); durable
/// per-response Message rows are summed independently with NULLs ignored
/// (c-usagefu: codex reports per-call partial baskets, and an omitted bucket
/// on one response is not the same as a missing Turn frame). Token buckets
/// are therefore `U1-sum(turn/stock) + SUM(messages)`. An ambiguous turn
/// poisons the turn/stock half only; a sticky-Unknown stock cost poisons; an
/// unpriced TURN on a turn-only (Claude) instance poisons, while durable
/// Message costs always sum (unpriced = 0).
fn aggregate_select_sql() -> &'static str {
    "SELECT
        (SELECT COALESCE(SUM(n), 0) FROM (
            SELECT COUNT(*) AS n FROM flow_turns
            UNION ALL SELECT COUNT(*) FROM flow_messages
            UNION ALL SELECT COUNT(*) FROM stock_sessions
            UNION ALL SELECT COUNT(*) FROM legacy_stock
            UNION ALL SELECT COUNT(*) FROM cost_only_reported
        )),
        (SELECT COUNT(*) FROM ambiguous_turns),
        (SELECT CASE
                 WHEN EXISTS (
                     SELECT 1 FROM ambiguous_turns WHERE total_tokens IS NOT NULL
                 ) THEN NULL
                 -- AA3: empty turn/stock half -> message sum when messages
                 -- exist or no cost-only carrier; Unknown for cost-only.
                 WHEN COUNT(*) = 0
                      AND (SELECT COUNT(*) FROM flow_messages) > 0 THEN 0
                 WHEN COUNT(*) = 0
                      AND NOT EXISTS (SELECT 1 FROM cost_only_reported) THEN 0
                 WHEN COUNT(*) = 0 THEN NULL
                 WHEN COUNT(*) = COUNT(total_tokens) THEN SUM(total_tokens)
             END
            FROM (
                SELECT total_tokens FROM flow_turns
                UNION ALL SELECT total_tokens FROM stock_sessions
                UNION ALL SELECT total_tokens FROM legacy_stock
            )
        )
        + COALESCE((SELECT SUM(total_tokens) FROM flow_messages), 0),
        + (SELECT CASE
                 WHEN EXISTS (
                     SELECT 1 FROM ambiguous_turns WHERE input_tokens IS NOT NULL
                 ) THEN NULL
                 -- AA3: with no turn/stock rows the value is the message
                 -- sum (0 when there are no messages AND no cost-only
                 -- carrier, but NULL when only a cost-only bill exists).
                 WHEN COUNT(*) = 0
                      AND (SELECT COUNT(*) FROM flow_messages) > 0 THEN 0
                 WHEN COUNT(*) = 0
                      AND NOT EXISTS (SELECT 1 FROM cost_only_reported) THEN 0
                 WHEN COUNT(*) = 0 THEN NULL
                 WHEN COUNT(*) = COUNT(input_tokens) THEN SUM(input_tokens)
             END
            FROM (
                SELECT input_tokens FROM flow_turns
                UNION ALL SELECT input_tokens FROM stock_sessions
                UNION ALL SELECT input_tokens FROM legacy_stock
            )
        )
        + COALESCE((SELECT SUM(input_tokens) FROM flow_messages), 0),
        + (SELECT CASE
                 WHEN EXISTS (
                     SELECT 1 FROM ambiguous_turns WHERE output_tokens IS NOT NULL
                 ) THEN NULL
                 -- AA3: with no turn/stock rows the value is the message
                 -- sum (0 when there are no messages AND no cost-only
                 -- carrier, but NULL when only a cost-only bill exists).
                 WHEN COUNT(*) = 0
                      AND (SELECT COUNT(*) FROM flow_messages) > 0 THEN 0
                 WHEN COUNT(*) = 0
                      AND NOT EXISTS (SELECT 1 FROM cost_only_reported) THEN 0
                 WHEN COUNT(*) = 0 THEN NULL
                 WHEN COUNT(*) = COUNT(output_tokens) THEN SUM(output_tokens)
             END
            FROM (
                SELECT output_tokens FROM flow_turns
                UNION ALL SELECT output_tokens FROM stock_sessions
                UNION ALL SELECT output_tokens FROM legacy_stock
            )
        )
        + COALESCE((SELECT SUM(output_tokens) FROM flow_messages), 0),
        + (SELECT CASE
                 WHEN EXISTS (
                     SELECT 1 FROM ambiguous_turns WHERE cache_read_tokens IS NOT NULL
                 ) THEN NULL
                 -- AA3: with no turn/stock rows the value is the message
                 -- sum (0 when there are no messages AND no cost-only
                 -- carrier, but NULL when only a cost-only bill exists).
                 WHEN COUNT(*) = 0
                      AND (SELECT COUNT(*) FROM flow_messages) > 0 THEN 0
                 WHEN COUNT(*) = 0
                      AND NOT EXISTS (SELECT 1 FROM cost_only_reported) THEN 0
                 WHEN COUNT(*) = 0 THEN NULL
                 WHEN COUNT(*) = COUNT(cache_read_tokens) THEN SUM(cache_read_tokens)
             END
            FROM (
                SELECT cache_read_tokens FROM flow_turns
                UNION ALL SELECT cache_read_tokens FROM stock_sessions
                UNION ALL SELECT cache_read_tokens FROM legacy_stock
            )
        )
        + COALESCE((SELECT SUM(cache_read_tokens) FROM flow_messages), 0),
        + (SELECT CASE
                 WHEN EXISTS (
                     SELECT 1 FROM ambiguous_turns WHERE cache_write_tokens IS NOT NULL
                 ) THEN NULL
                 -- AA3: with no turn/stock rows the value is the message
                 -- sum (0 when there are no messages AND no cost-only
                 -- carrier, but NULL when only a cost-only bill exists).
                 WHEN COUNT(*) = 0
                      AND (SELECT COUNT(*) FROM flow_messages) > 0 THEN 0
                 WHEN COUNT(*) = 0
                      AND NOT EXISTS (SELECT 1 FROM cost_only_reported) THEN 0
                 WHEN COUNT(*) = 0 THEN NULL
                 WHEN COUNT(*) = COUNT(cache_write_tokens) THEN SUM(cache_write_tokens)
             END
            FROM (
                SELECT cache_write_tokens FROM flow_turns
                UNION ALL SELECT cache_write_tokens FROM stock_sessions
                UNION ALL SELECT cache_write_tokens FROM legacy_stock
            )
        )
        + COALESCE((SELECT SUM(cache_write_tokens) FROM flow_messages), 0),
        -- r38 item 3 / r39 items 2-4 (AA2/AA3): cost is composed PER
        -- ATTRIBUTABLE INSTANCE. An instance with a native REPORTED cumulative
        -- bill contributes exactly those bills (newest KNOWN per session,
        -- summed across its sessions) — its per-call/stock estimates are not
        -- added and, crucially, an unrelated instance's reported bill never
        -- suppresses another instance's estimates. Everything else bills the
        -- existing Ruling-B/U1 estimate (sticky-Unknown stock and unpriced
        -- turn-only poison preserved). Presence is independent of tokens: a
        -- cost-only reported carrier keeps this column non-NULL even when its
        -- model has zero token contributions (AA3).
        CASE
            WHEN (SELECT COUNT(*) FROM (
                SELECT 1 FROM flow_turns
                UNION ALL SELECT 1 FROM flow_messages
                UNION ALL SELECT 1 FROM stock_sessions
                UNION ALL SELECT 1 FROM legacy_stock
                UNION ALL SELECT 1 FROM cost_only_reported
            )) = 0 THEN NULL
            -- Ambiguous turn cost poisons only instances WITHOUT reported
            -- authority (the reported bill is authoritative by definition).
            WHEN EXISTS (
                SELECT 1 FROM ambiguous_turns WHERE cost_usd IS NOT NULL
                  AND instance_id NOT IN (SELECT instance_id FROM reported_instances)
            ) THEN NULL
            -- Sticky-Unknown stock poisons the non-reported estimate.
            WHEN EXISTS (
                SELECT 1 FROM (
                    SELECT instance_id, cost_usd FROM stock_sessions
                    UNION ALL
                    SELECT instance_id, cost_usd FROM legacy_stock
                ) est
                WHERE est.cost_usd IS NULL
                  AND est.instance_id NOT IN (SELECT instance_id FROM reported_instances)
            ) THEN NULL
            -- An unpriced turn on a turn-only instance poisons only when that
            -- instance has no reported bill.
            WHEN EXISTS (
                SELECT 1 FROM flow_turns ft
                WHERE ft.cost_usd IS NULL
                  AND ft.instance_id NOT IN (SELECT instance_id FROM reported_instances)
                  AND NOT EXISTS (
                      SELECT 1 FROM usage_events sx
                      WHERE sx.instance_id = ft.instance_id
                        AND sx.scope = 'session'
                  )
            ) THEN NULL
            ELSE
                -- Reported cumulative bills, once per attributable session.
                COALESCE((
                    SELECT SUM(CAST(cost_usd AS REAL)) FROM reported_session_cost
                ), 0.0)
                -- Non-reported instances: selected stock estimates.
                + COALESCE((
                    SELECT SUM(CAST(cost_usd AS REAL)) FROM (
                        SELECT cost_usd FROM stock_sessions
                        WHERE instance_id NOT IN (SELECT instance_id FROM reported_instances)
                        UNION ALL
                        SELECT cost_usd FROM legacy_stock
                        WHERE instance_id NOT IN (SELECT instance_id FROM reported_instances)
                    )
                ), 0.0)
                -- Non-reported per-turn flow.
                + COALESCE((
                    SELECT SUM(CAST(cost_usd AS REAL)) FROM flow_turns
                    WHERE instance_id NOT IN (SELECT instance_id FROM reported_instances)
                ), 0.0)
                -- Durable Message flow: an unpriced Message contributes 0
                -- (Ruling B per-call billing, c-usagefu r10).
                + COALESCE((
                    SELECT SUM(CAST(cost_usd AS REAL)) FROM flow_messages
                    WHERE instance_id NOT IN (SELECT instance_id FROM reported_instances)
                ), 0.0)
        END
     -- No FROM: every column is a scalar subquery over the fold CTEs, so the
     -- row must materialize even when `contributions` is empty (an empty
     -- aggregate SELECT over a zero-row FROM would otherwise return no rows).
     "
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
    })
}

/// Aggregate all persisted events for one instance through the unified fold.
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

/// Aggregate persisted events for one `(profile, model)`. Turn winners are
/// ranked GLOBALLY per `(instance_id, scope_id)` BEFORE the supply filter
/// (c-ctxusage r18 item 2): a row superseded by a newer accepted frame for the
/// same message id under another model must not keep contributing here.
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
        })
    })
}
/// Per-session token/context rollup backing the composer's context chip
/// popover (context-usage-1).
///
/// An additive projection of `usage_events`, recomputed on read, so the TPM
/// windows never go stale. Every counter is `None` until at least one usage
/// observation has reported it: adapters that do not emit a channel (Codex
/// cache fields, Grok uncached breakdown) leave it unknown — the client
/// renders `—`, never a fabricated zero.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceUsageRollup {
    /// c-usagefu r16 item 7: the store is on the rolled-back/legacy shape, so
    /// none of the counters below could be projected. The client renders this
    /// as Unknown (never 0 and never a kind-guessed percentage). Distinct from
    /// `rollup_instance` returning `None`, which means there is NO rollup
    /// channel / no usage at all.
    pub unavailable: bool,
    /// Tokens the next request will carry: the last turn's fresh input +
    /// cache read + cache creation. `None` when no component was reported.
    pub context_used_tokens: Option<i64>,
    /// Context window size in tokens (model catalog / `[1m]` tag / kind).
    pub context_window_tokens: Option<i64>,
    /// `context_used / context_window`, rounded and clamped to 0..=100.
    pub context_pct: Option<i64>,
    /// True when `context_window_tokens` came only from the harness-kind
    /// fallback (unknown model): the percentage is then an estimate and the
    /// UI marks it `≈`. False for native/profile/catalog evidence.
    pub context_pct_approximate: bool,
    /// Sum of fresh (uncached) input tokens across the session.
    pub session_input_tokens: Option<i64>,
    /// Sum of output tokens across the session.
    pub session_output_tokens: Option<i64>,
    /// Sum of prompt-cache reads across the session.
    pub cache_read_tokens: Option<i64>,
    /// Sum of prompt-cache creations (writes) across the session.
    pub cache_creation_tokens: Option<i64>,
    /// Number of usage observations folded (one per turn for Claude).
    pub turns: i64,
    /// Fresh input tokens observed during the last 60 seconds.
    pub tpm_in_60s: Option<i64>,
    /// Output tokens observed during the last 60 seconds.
    pub tpm_out_60s: Option<i64>,
    /// Average per-minute input rate over the last 5 minutes.
    pub tpm_in_5m: Option<i64>,
    /// Average per-minute output rate over the last 5 minutes.
    pub tpm_out_5m: Option<i64>,
    /// Observed-at of the most recent usage event.
    pub last_turn_at: Option<String>,
}

/// RFC3339 UTC millisecond timestamp `seconds` in the past, byte-comparable
/// with the stamps `Store::append_journal` writes (both are fixed-width
/// `…Z`), so the TPM window predicates are plain string comparisons.
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

/// Sum the per-call token FLOW within a rate window (r9 item 4: the shared
/// call-identity relation — every Message plus unmatched Turns, legacy
/// NULL-scope Turns included). Session snapshots are a CUMULATIVE STOCK
/// replaced in place, never a per-window flow, so they are excluded here; the
/// caller substitutes timestamped stock increments only when the harness
/// emits no native flow (Grok) — c-ctxusage r3 item 6 / r4 item 6.
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

/// Per-bucket flow derived from two successive cumulative session snapshots.
#[derive(Clone, Default)]
struct StockIncrement {
    input: Option<i64>,
    output: Option<i64>,
    end_at: String,
}

/// Timestamped flow between successive cumulative Session snapshots, in native
/// source order.
///
/// c-ctxusage r4 item 6: a cumulative stock is NOT throughput ("1000 tokens an
/// hour ago + 10 now" must not read as "1010 in the last minute"). The rate is
/// the sum of increments whose END snapshot landed inside the window:
/// `increment[bucket] = max(0, current - previous)` — the saturating diff is
/// the explicit reset handling. With a single snapshot there is no
/// predecessor, so no trustworthy increment exists and the rate is unknown
/// (never the stock itself).
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

/// One ordered cumulative-session point: the two rate buckets, its observed
/// time, the session scope id (NULL for legacy rows) and the time source.
type SessionPointRow = (Option<i64>, Option<i64>, String, Option<String>, String);

/// Ordered session points; scoped or legacy NULL-scope only. EVERY accepted
/// point is walked — including ingest-stamped historical replays — because a
/// timestamp-free replay still ADVANCES the cumulative baseline: skipping it
/// made the next native point diff against a stale stock and inflate a single
/// increment (native 100 → untimed 120, 140 → native 150 read as +50 instead
/// of the true +10). Only the increment EMISSION is source-gated
/// (c-ctxusage r9 item 3).
///
/// c-ctxusage r10 item 3: the chain order is the ACCEPTED order (insertion
/// `rowid`), never `observed_at`: a buffered native point carrying an EARLIER
/// timestamp can be accepted AFTER ingest-stamped rows, and ordering by the
/// mixed clocks would both pick the wrong predecessor and promote a stale
/// ingest stock to "current". Native timestamps only place the resulting
/// increment into a rate window (see [`diff_rows`]); they do not order the
/// cumulative sequence.
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

/// Diff ordered points, restarting the chain at every scope boundary. Every
/// point advances the predecessor baseline; an increment is emitted ONLY for
/// a point whose timestamp is time-authoritative against its IMMEDIATE
/// predecessor, and only when the predecessor exists. Two provenances carry
/// authority: a post-upgrade `"native"` timestamp, and the migration-default
/// `"legacy"` source stamped exclusively on databases from BEFORE the source
/// column existed (c-usagefu r8 item 5: no provenance existed, the retained
/// timestamp is the only evidence). An explicit `"ingest"` point — including
/// a pre-upgrade replay retained through a database that already had the
/// column — is NEVER trusted: its time is the ingest clock of a
/// timestamp-free replay, and the lineage upgrade tag does not change that
/// (r37 item 3). A native point with no predecessor, or an interval whose
/// predecessor counters are unknown, yields no increment — the interval is
/// unknown, never the stock (c-ctxusage r9 item 3). An unknown PREDECESSOR
/// bucket stays unknown rather than being treated as zero: unknown→150
/// cannot be read as a 150 increment (c-ctxusage r10 item 4).
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

/// Sum session-stock increments whose end snapshot landed at/after `since`.
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

/// One session's latest cumulative point: the four counters and its observed
/// time, one row per scope id.
type LatestStockPoint = (Option<i64>, Option<i64>, Option<i64>, Option<i64>, String);

/// The summed current cumulative stock across session ids. Timekeeping is
/// split: `native_observed_at` is the newest NATIVE session time and is the
/// only value allowed to drive lastTurnAt / rate windows (c-ctxusage r8
/// item 9); `latest_observed_at` may be an ingest fallback and is used only
/// for ordering/display that cannot trust freshness.
#[derive(Debug, Default, Clone)]
struct SessionStock {
    input: Option<i64>,
    output: Option<i64>,
    cache_read: Option<i64>,
    cache_write: Option<i64>,
    native_observed_at: Option<String>,
    latest_observed_at: Option<String>,
}

/// The latest cumulative session growth point for EACH scope id (c-ctxusage
/// r5 item 3A): an instance can run codex session s1 and later s2, and totals
/// must be the sum of both sessions' current stocks — never the newest row
/// alone (which would make totals "drop" at the s1→s2 boundary).
///
/// When no scoped rows exist, the single newest LEGACY NULL-scope row is
/// returned (pre-`scopeId` world).
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

/// Sum two constituent session buckets under U1 (c-ctxusage r17 item 6): the
/// total is Known only when EVERY constituent session reports the bucket. An
/// unknown session bucket makes the folded total Unknown — it is never taken
/// as zero and the known sessions alone can never produce a total.
fn add_optional(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        _ => None,
    }
}

/// The newest per-call basket: fresh input plus every cached bucket of the
/// most recent model call — what the NEXT request will carry.
///
/// Scope preference (c-usagefu (b)): a per-REQUEST `message` row (one model
/// response, Codex) is the most precise; a per-`turn` snapshot (Claude
/// message id, and Codex's end-of-turn snapshot) is the fallback. A cumulative
/// session row never feeds the context figure (it spans the whole session).
///
/// c-ctxusage r4 item 4: "newest" is the NATIVE time, tie-broken by ingest
/// seq — a late historical correction at a high seq never becomes current.
/// c-ctxusage r4 item 4 / r17 item 6 / r18 item 1: the context basket is the
/// newest surviving flow call — a lineage WINNER turn ([`unified_fold_ctes`]
/// already folded superseded lineages) or an eligible Message — never a raw
/// superseded turn row and never a cumulative session row. "Newest" is the
/// reported time, tie-broken by seq. U1: the sum is Known only when that call
/// reported input AND both cache buckets; one omission returns Unknown rather
/// than summing the known baskets.
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

/// U1 stock-only context: Known only when the current stock reports input and
/// BOTH cache buckets (c-ctxusage r17 item 6).
fn known_context_total(baskets: [Option<i64>; 3]) -> Option<i64> {
    baskets
        .into_iter()
        .collect::<Option<Vec<i64>>>()
        .map(|known| known.into_iter().fold(0, i64::saturating_add))
}

/// The newest native per-model context window reported on a usage row.
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

/// Inputs to [`rollup_instance`] beyond the connection: what it knows about
/// the session's model so it can resolve a context window.
pub struct RollupRequest<'a> {
    pub instance_id: &'a str,
    pub kind: &'a str,
    /// Launch model (`instances.model`).
    pub spec_model: Option<&'a str>,
    /// Transcript-observed effective model id (`spec.modelEffective.id`,
    /// i.e. after a `/model` switch).
    pub effective_model: Option<&'a str>,
    pub profile_id: Option<&'a str>,
}

/// Kind-level fallback context windows. Generic/terminal kinds report nothing.
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

/// The static catalog's window for a model id. The explicit `[1m]` tag is
/// resolved separately and earlier (it outranks the profile); this is only the
/// catalog lookup (c-usagefu r2 item 4).
fn catalog_window(model: &str) -> Option<i64> {
    crate::model_catalog::lookup(model.trim()).map(|row| row.context_window as i64)
}

/// A context window declared on the provider profile's `models_json`.
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
    /// True when a gateway-renamed id (`gw/model[1m]`) declares the long-context
    /// tag for `wanted`, allowing exact profile match semantics against the
    /// profile's own id spelling.
    pub(super) fn tag_matches(entry_id: &str, wanted: &str) -> bool {
        let no_tag = wanted.trim_end_matches("[1m]").trim();
        super::is_one_million_tag(entry_id)
            && (entry_id == wanted
                || entry_id.ends_with(&format!("/{wanted}"))
                || entry_id.ends_with(&format!("/{no_tag}")))
    }
}

/// Resolve a session's context window and whether the number is an estimate.
///
/// Native harness report (payload `contextWindow`, e.g. stream-json
/// `modelUsage`) wins. Otherwise the model candidate and its sources resolve,
/// in this order per candidate:
///
/// 1. the explicit `[1m]` long-context tag on the id (exact `1_000_000`);
/// 2. the provider profile's declared `models_json` `contextWindow` — a
///    gateway serving a model at its own window must not be overridden by the
///    static catalog (c-usagefu r2 item 4);
/// 3. the static catalog row.
///
/// The candidate itself is the transcript-observed effective id
/// (`modelEffective`, i.e. after a `/model` switch) when one is known; the
/// launch `spec.model` is consulted ONLY when no effective model exists. An
/// effective model that resolves to nothing must not fall back to the launch
/// model's window marked exact — it goes to the harness-kind fallback, which is
/// the single approximate step (a guess at an unknown gateway model's window).
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

/// The harness-kind fallback window, always approximate.
fn kind_fallback(kind: &str) -> (Option<i64>, bool) {
    match kind_context_window(kind) {
        Some(window) => (Some(window), true),
        None => (None, false),
    }
}

/// Session-wide totals of one rollup query.
struct RollupTotals {
    turns: i64,
    /// Additive sums over the ELIGIBLE flow rows (r9 items 4/5): every
    /// Message plus Turn rows no matching Message represents. Used for
    /// totals whenever the instance has any eligible flow.
    turn_input: Option<i64>,
    turn_output: Option<i64>,
    turn_cache_read: Option<i64>,
    turn_cache_write: Option<i64>,
    has_session: bool,
    last_turn_at: Option<String>,
}

/// Fold the instance's session stocks and uncovered turn flows into the
/// rollup's session totals (c-ctxusage r28 item 1 / r29 items 1-2).
///
/// Uses the SAME [`FOLD_CTES_SQL`] rule as the aggregates: latest stock per
/// native session, newest legacy point only with no scoped stock, proven
/// uncovered turns, and Unknown poisoning when an unprovable turn sits beside
/// scoped stock. U1 COUNT(col)=COUNT(*) gating keeps an omitted bucket
/// Unknown.
///
/// Fold every persisted usage event of one instance into
/// [`InstanceUsageRollup`]. Returns `None` when the session has no usage
/// observations yet, so the field stays off the instance record entirely.
///
/// Stock vs flow (c-usagefu r9 items 4/5):
/// - A **Session** row is an append-only cumulative STOCK growth point
///   (Grok/Codex); the latest accepted point per instance/session is the
///   current stock.
/// - **Message** rows are per-call flow (Codex) and **Turn** rows are
///   per-call flow (Claude; legacy codex too): ONE shared eligible-flow
///   relation by call identity keeps every Message and drops only Turns a
///   matching Message already represents.
///
/// Totals are the eligible flow sums whenever ANY eligible flow exists; the
/// latest cumulative stock is used ONLY when no eligible flow exists — stock
/// is never added to flow and never used as a smaller-flow fallback. The
/// context basket is the newest message/turn call, falling back to the
/// stock. Rate windows sum native flow; a stock-only harness contributes
/// timestamped increments between successive growth points (never the whole
/// stock).
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
                -- U1 gate over the Turn/stock accounting frames only; durable
                -- per-response Messages sum independently (NULLs ignored).
                (SELECT CASE
                         WHEN EXISTS (
                             SELECT 1 FROM ambiguous_turns
                             WHERE input_tokens IS NOT NULL
                         ) THEN NULL
                         WHEN COUNT(*) = 0 THEN 0
                         WHEN COUNT(*) = COUNT(input_tokens) THEN SUM(input_tokens)
                     END
                    FROM (
                        SELECT input_tokens FROM flow_turns
                        UNION ALL SELECT input_tokens FROM stock_sessions
                        UNION ALL SELECT input_tokens FROM legacy_stock
                    )
                )
                + COALESCE((SELECT SUM(input_tokens) FROM flow_messages), 0),
                (SELECT CASE
                         WHEN EXISTS (
                             SELECT 1 FROM ambiguous_turns
                             WHERE output_tokens IS NOT NULL
                         ) THEN NULL
                         WHEN COUNT(*) = 0 THEN 0
                         WHEN COUNT(*) = COUNT(output_tokens) THEN SUM(output_tokens)
                     END
                    FROM (
                        SELECT output_tokens FROM flow_turns
                        UNION ALL SELECT output_tokens FROM stock_sessions
                        UNION ALL SELECT output_tokens FROM legacy_stock
                    )
                )
                + COALESCE((SELECT SUM(output_tokens) FROM flow_messages), 0),
                (SELECT CASE
                         WHEN EXISTS (
                             SELECT 1 FROM ambiguous_turns
                             WHERE cache_read_tokens IS NOT NULL
                         ) THEN NULL
                         WHEN COUNT(*) = 0 THEN 0
                         WHEN COUNT(*) = COUNT(cache_read_tokens) THEN SUM(cache_read_tokens)
                     END
                    FROM (
                        SELECT cache_read_tokens FROM flow_turns
                        UNION ALL SELECT cache_read_tokens FROM stock_sessions
                        UNION ALL SELECT cache_read_tokens FROM legacy_stock
                    )
                )
                + COALESCE((SELECT SUM(cache_read_tokens) FROM flow_messages), 0),
                (SELECT CASE
                         WHEN EXISTS (
                             SELECT 1 FROM ambiguous_turns
                             WHERE cache_write_tokens IS NOT NULL
                         ) THEN NULL
                         WHEN COUNT(*) = 0 THEN 0
                         WHEN COUNT(*) = COUNT(cache_write_tokens) THEN SUM(cache_write_tokens)
                     END
                    FROM (
                        SELECT cache_write_tokens FROM flow_turns
                        UNION ALL SELECT cache_write_tokens FROM stock_sessions
                        UNION ALL SELECT cache_write_tokens FROM legacy_stock
                    )
                )
                + COALESCE((SELECT SUM(cache_write_tokens) FROM flow_messages), 0),
                (SELECT MAX(observed_at) FROM (
                    SELECT observed_at FROM flow_turns
                     WHERE observed_at_source = 'native'
                    UNION ALL
                    SELECT observed_at FROM flow_messages
                     WHERE observed_at_source = 'native'
                 ))"
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
    /// The explicit Unavailable rollup for a store the usage migration did not
    /// upgrade (c-usagefu r16 item 7). Every counter is unknown; the client
    /// renders Unknown rather than zero or a kind-fallback percentage.
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

/// Observe a fresh journal append: project + persist usage events. Called from
/// the `journal.append` RPC path alongside `alerts::observe`. A replay never
/// reaches here (the caller gates on `JournalAppend.replayed`).
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

    #[test]
    fn migration_consolidates_duplicate_message_rows_idempotently() {
        // c-usagefu r9 item 3: a pre-r9 DB holds duplicate Message rows from
        // byte-0 replays. The FIRST write is an ingest-time row with unknown
        // counters/cost and no turn id; a LATER replay carries the native
        // time, turn id and known counters/cost. migrate() must consolidate
        // the durable key to ONE survivor with every best attribute, delete
        // the rest and create the unique index in a single transaction.
        let conn = Connection::open_in_memory().unwrap();
        // Minimal table that already has scope_id + turn_id + source columns
        // (the duplicate problem arose after r7 added them but before r9's
        // consolidation); no unique index yet.
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
        // r1: seq 1 first (ingest time, everything unknown); seq 2 the later
        // replay (native time, turn id, known counters/cost). r2 single row.
        conn.execute_batch(
            "INSERT INTO usage_events
                (instance_id, seq, scope, scope_id, turn_id, mode,
                 total_tokens, input_tokens, output_tokens,
                 cache_read_tokens, cache_write_tokens, cost_usd,
                 observed_at, observed_at_source, profile_id, model)
             VALUES
                ('ins_dup', 1, 'message', 'r1', NULL, 'snapshot',
                 NULL, NULL, NULL, NULL, NULL, NULL,
                 '2026-09-15T00:00:00.000Z', 'ingest', NULL, NULL),
                ('ins_dup', 2, 'message', 'r1', 't1', 'snapshot',
                 155, 40, 10, 100, 5, '0.10',
                 '2026-09-15T00:00:05.000Z', 'native', 'pvp_dup', 'gpt-5.4'),
                ('ins_dup', 3, 'message', 'r2', 't2', 'snapshot',
                 210, 60, 20, 150, 7, '0.20',
                 '2026-09-15T00:00:10.000Z', 'native', 'pvp_dup', 'gpt-5.4');",
        )
        .unwrap();

        migrate(&conn).unwrap();

        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "r1 and r2 each survive once (was 3)");
        // The COMPLETE surviving r1 row: first write's seq, later replay's
        // every attribute.
        #[derive(Debug, PartialEq)]
        struct R1Survivor {
            seq: i64,
            profile_id: Option<String>,
            model: Option<String>,
            turn_id: Option<String>,
            total: Option<i64>,
            input: Option<i64>,
            output: Option<i64>,
            cache_read: Option<i64>,
            cache_write: Option<i64>,
            cost: Option<String>,
            observed_at: String,
            source: String,
        }
        let r1 = conn
            .query_row(
                "SELECT seq, profile_id, model, turn_id,
                        total_tokens, input_tokens, output_tokens,
                        cache_read_tokens, cache_write_tokens, cost_usd,
                        observed_at, observed_at_source
                 FROM usage_events WHERE scope_id='r1'",
                [],
                |r| {
                    Ok(R1Survivor {
                        seq: r.get(0)?,
                        profile_id: r.get(1)?,
                        model: r.get(2)?,
                        turn_id: r.get(3)?,
                        total: r.get(4)?,
                        input: r.get(5)?,
                        output: r.get(6)?,
                        cache_read: r.get(7)?,
                        cache_write: r.get(8)?,
                        cost: r.get(9)?,
                        observed_at: r.get(10)?,
                        source: r.get(11)?,
                    })
                },
            )
            .unwrap();
        assert_eq!(r1.seq, 1, "the first write's row is the survivor");
        assert_eq!(r1.profile_id.as_deref(), Some("pvp_dup"));
        assert_eq!(r1.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(r1.turn_id.as_deref(), Some("t1"), "best turn_id");
        assert_eq!(r1.total, Some(155), "known total from the later replay");
        assert_eq!(r1.input, Some(40));
        assert_eq!(r1.output, Some(10));
        assert_eq!(r1.cache_read, Some(100));
        assert_eq!(r1.cache_write, Some(5));
        assert_eq!(r1.cost.as_deref(), Some("0.10"), "known cost not lost");
        assert_eq!(
            r1.observed_at, "2026-09-15T00:00:05.000Z",
            "native time replaces the ingest time"
        );
        assert_eq!(r1.source, "native");
        // r2 untouched.
        let (r2_out, r2_cost): (Option<i64>, Option<String>) = conn
            .query_row(
                "SELECT output_tokens, cost_usd FROM usage_events WHERE scope_id='r2'",
                [],
                |r| Ok((r.get(0).unwrap(), r.get(1).unwrap())),
            )
            .unwrap();
        assert_eq!(r2_out, Some(20));
        assert_eq!(r2_cost.as_deref(), Some("0.20"));

        // Idempotent: a second migrate changes nothing (count + r1 values).
        migrate(&conn).unwrap();
        let n2: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n2, 2);
        let r1_recheck: (Option<String>, Option<i64>, String) = conn
            .query_row(
                "SELECT turn_id, total_tokens, observed_at FROM usage_events WHERE scope_id='r1'",
                [],
                |r| Ok((r.get(0).unwrap(), r.get(1).unwrap(), r.get(2).unwrap())),
            )
            .unwrap();
        assert_eq!(
            r1_recheck,
            (
                Some("t1".to_string()),
                Some(155),
                "2026-09-15T00:00:05.000Z".to_string()
            )
        );

        // The unique index exists and rejects a new duplicate.
        let dup_insert = conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, scope, scope_id, turn_id, mode, observed_at)
             VALUES ('ins_dup', 99, 'message', 'r1', 't1', 'snapshot', 'x')",
            [],
        );
        assert!(
            dup_insert.is_err(),
            "unique message index blocks duplicates"
        );
    }

    #[test]
    fn migration_accounting_comes_from_the_highest_revision_survivor() {
        // c-usagefu r10 item 3 (Ruling C), made exact by r12 item 5 (C1'):
        // BOTH duplicates carry known accounting at DIFFERENT revisions —
        // rev 1 reports total 10 / input 7 / output 10 / cost 0.01 /
        // estimated, a rev-2 replay reports total 20 / input NULL / output
        // 20 / cost 0.02 / reported. Counters/cost/accounting come ONLY from
        // the highest revision, so the survivor takes rev-2 values and its
        // input stays NULL — rev-1's 7 never fills a rev-2 gap. Attribution
        // (turn id, native time, profile, model, mode, context window) merges
        // best-known from either revision. A second migration is a no-op.
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
        conn.execute_batch(
            "INSERT INTO usage_events
                (instance_id, seq, scope, scope_id, turn_id, mode, metric_revision,
                 total_tokens, input_tokens, output_tokens,
                 cache_read_tokens, cache_write_tokens, cost_usd,
                 observed_at, observed_at_source, profile_id, model,
                 native_context_window, accounting)
             VALUES
                ('ins_rev', 1, 'message', 'r1', NULL, 'snapshot', 1,
                 10, 7, 10, NULL, NULL, '0.01',
                 '2026-09-15T00:00:00.000Z', 'ingest', NULL, NULL, NULL,
                 'estimated'),
                ('ins_rev', 2, 'message', 'r1', 't1', 'snapshot', 2,
                 20, NULL, 20, 50, 3, '0.02',
                 '2026-09-15T00:00:05.000Z', 'native', 'pvp_rev', 'gpt-5.4',
                 200000, 'reported');",
        )
        .unwrap();

        #[derive(Debug, PartialEq)]
        struct Survivor {
            seq: i64,
            revision: i64,
            mode: String,
            total: Option<i64>,
            input: Option<i64>,
            output: Option<i64>,
            cache_read: Option<i64>,
            cache_write: Option<i64>,
            cost: Option<String>,
            accounting: String,
            turn_id: Option<String>,
            profile: Option<String>,
            model: Option<String>,
            context_window: Option<i64>,
            observed_at: String,
            source: String,
        }
        let read_survivor = |conn: &Connection| -> Survivor {
            conn.query_row(
                "SELECT seq, metric_revision, mode,
                        total_tokens, input_tokens, output_tokens,
                        cache_read_tokens, cache_write_tokens, cost_usd,
                        accounting,
                        turn_id, profile_id, model, native_context_window,
                        observed_at, observed_at_source
                 FROM usage_events WHERE scope_id='r1'",
                [],
                |r| {
                    Ok(Survivor {
                        seq: r.get(0)?,
                        revision: r.get(1)?,
                        mode: r.get(2)?,
                        total: r.get(3)?,
                        input: r.get(4)?,
                        output: r.get(5)?,
                        cache_read: r.get(6)?,
                        cache_write: r.get(7)?,
                        cost: r.get(8)?,
                        accounting: r.get(9)?,
                        turn_id: r.get(10)?,
                        profile: r.get(11)?,
                        model: r.get(12)?,
                        context_window: r.get(13)?,
                        observed_at: r.get(14)?,
                        source: r.get(15)?,
                    })
                },
            )
            .unwrap()
        };

        migrate(&conn).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "the two revisions consolidate to one survivor");
        let first = read_survivor(&conn);
        assert_eq!(first.seq, 1, "the first write's row remains the survivor");
        assert_eq!(first.revision, 2, "revision is the authoritative max");
        assert_eq!(first.total, Some(20), "rev-2 total wins over rev-1 10");
        assert_eq!(
            first.input, None,
            "C1': rev-2 leaves input NULL and rev-1's known 7 never fills it"
        );
        assert_eq!(first.output, Some(20), "rev-2 output 20 wins over rev-1 10");
        assert_eq!(first.cache_read, Some(50));
        assert_eq!(first.cache_write, Some(3));
        assert_eq!(
            first.cost.as_deref(),
            Some("0.02"),
            "cost is accounting: rev-2 0.02 wins, never the first non-NULL 0.01"
        );
        assert_eq!(
            first.accounting, "reported",
            "accounting is accounting: rev-2 'reported' wins over rev-1 'estimated'"
        );
        // Attribution merges best-known from either revision.
        assert_eq!(first.turn_id.as_deref(), Some("t1"));
        assert_eq!(first.profile.as_deref(), Some("pvp_rev"));
        assert_eq!(first.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(first.context_window, Some(200000));
        assert_eq!(first.mode, "snapshot");
        assert_eq!(first.observed_at, "2026-09-15T00:00:05.000Z");
        assert_eq!(first.source, "native");

        // A second migration is a no-op: the COMPLETE survivor is unchanged.
        migrate(&conn).unwrap();
        let count2: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count2, 1);
        assert_eq!(read_survivor(&conn), first, "re-migrating changes nothing");
    }

    #[test]
    fn migration_keeps_a_top_revision_null_even_when_an_older_revision_was_known() {
        // c-usagefu r12 item 5 (Ruling C1' exact): determine the highest
        // revision FIRST; counters, cost and accounting come ONLY from that
        // revision. A rev-1 input 7 / cost 0.10 / estimated and a rev-2
        // NULL input / NULL cost / reported leave the survivor at rev 2 with
        // input and cost STILL NULL — the older revision's values never fill
        // them. NULL repair happens only among rows AT the top revision:
        // a second rev-2 row carrying input 9 repairs the input to 9, while
        // the rev-1 7 is ignored. Actual old-schema database, migrated.
        let conn = open_old_dual_schema();
        conn.execute_batch(
            "INSERT INTO usage_events
                (instance_id, seq, scope, scope_id, turn_id, mode,
                 metric_revision, total_tokens, input_tokens, output_tokens,
                 cost_usd, accounting, observed_at, observed_at_source)
             VALUES
                -- key 'c1p': rev 1 fully known; rev 2 NULL input/cost,
                -- with a same-revision sibling repairing input but not cost.
                ('ins_c1p', 1, 'message', 'c1p', 't', 'snapshot', 1,
                 100, 7, 93, '0.10', 'estimated',
                 '2026-09-15T00:00:00.000Z', 'native'),
                ('ins_c1p', 2, 'message', 'c1p', 't', 'snapshot', 2,
                 120, NULL, NULL, NULL, 'reported',
                 '2026-09-15T00:00:05.000Z', 'native'),
                ('ins_c1p', 3, 'message', 'c1p', 't', 'snapshot', 2,
                 NULL, 9, 111, NULL, 'reported',
                 '2026-09-15T00:00:06.000Z', 'native'),
                -- key 'c1n': rev 2 NULL cost with NO same-revision repair,
                -- and an OLDER revision carrying a STRONGER accounting
                -- authority ('reported') which must not win at rev 2. The
                -- rev-1 row arrives OUT OF ORDER (highest rowid) so a
                -- newest-rowid tiebreak without the revision filter would
                -- wrongly take 'reported'.
                ('ins_c1n', 4, 'message', 'c1n', 't', 'snapshot', 2,
                 60, 6, 54, NULL, 'estimated',
                 '2026-09-15T00:00:05.000Z', 'native'),
                ('ins_c1n', 5, 'message', 'c1n', 't', 'snapshot', 1,
                 50, 5, 45, '0.05', 'reported',
                 '2026-09-15T00:00:00.000Z', 'native');",
        )
        .unwrap();
        migrate(&conn).unwrap();

        let (revision, total, input, output, cost, accounting): (
            i64,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<String>,
            String,
        ) = conn
            .query_row(
                "SELECT metric_revision, total_tokens, input_tokens,
                        output_tokens, cost_usd, accounting
                 FROM usage_events WHERE instance_id='ins_c1p'",
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
        assert_eq!(revision, 2, "the highest revision is taken first");
        assert_eq!(total, Some(120), "total comes from rev 2 only");
        assert_eq!(
            input,
            Some(9),
            "NULL input is repaired ONLY from another rev-2 row, never rev-1 7"
        );
        assert_eq!(output, Some(111), "output comes from the rev-2 sibling");
        assert_eq!(
            cost, None,
            "rev-2 NULL cost stays NULL; rev-1 0.10 never fills it"
        );
        assert_eq!(accounting, "reported", "accounting is the rev-2 authority");

        let cost_n: Option<String> = conn
            .query_row(
                "SELECT cost_usd FROM usage_events WHERE instance_id='ins_c1n'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            cost_n, None,
            "a NULL cost with no same-revision repair stays NULL"
        );
        // Accounting follows the SAME at-top-revision rule: the top revision
        // is 2 ('estimated'); the older rev-1 'reported' never overrides it.
        let accounting_n: String = conn
            .query_row(
                "SELECT accounting FROM usage_events WHERE instance_id='ins_c1n'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            accounting_n, "estimated",
            "accounting comes from the top revision only, not a stronger rev 1"
        );

        // Idempotent: a second migration does not discover rev 1.
        migrate(&conn).unwrap();
        let cost_still_null: Option<String> = conn
            .query_row(
                "SELECT cost_usd FROM usage_events WHERE instance_id='ins_c1p'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cost_still_null, None);
    }

    /// Create a real pre-r6-schema usage_events table (all modern columns, no
    /// dedupe indexes) so Ruling B regressions migrate an ACTUAL historical
    /// database rather than inserting into an already-migrated one.
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
                insert_row(conn, &row).expect("codex usage row projects");
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

    #[test]
    fn a_main_shape_database_migrates_and_counts_once_after_two_real_byte_zero_replays() {
        // c-usagefu r13 item 2 (COORDINATOR RULING U1), regression (a): the
        // upgrade case that actually exists in the field. A database written by
        // origin/main's REAL usage_events schema and codex insert path holds
        // turn- and session-scope rows ONLY (main's projection persisted no
        // scope_id, though the durable journal retained payload.scopeId).
        // Migrate recovers the Turn scope_ids from the journal; then a REAL
        // CodexAdapter byte-zero replay re-tails the rollout TWICE, re-emitting
        // each call as a durable Message. Totals, cost and the TPM windows must
        // count every call EXACTLY ONCE and omit nothing: bucket-sum equality
        // suppresses the backfilled Turn rows in favour of the durable
        // Messages, and the durable unique index makes the second replay a
        // no-op.
        use remuda_driver::adapters::{
            AdapterHome, CodexAdapter, FileSignalAdapter, StampCtx, stamp,
        };
        use remuda_protocol::{InstanceId, ObservationPayload};
        use std::io::Write;

        let profile = "pvp_r13main";
        let model = "gpt-5.4";
        let session_id = "r13main";
        let instance_id = "ins_r13main";

        // Native model-call timestamps, recent enough for BOTH TPM windows.
        let rfc = |secs: i64| {
            let t = time::OffsetDateTime::now_utc() - time::Duration::seconds(secs);
            t.format(&time::format_description::well_known::Rfc3339)
                .unwrap()
        };
        let at1 = rfc(10);
        let at2 = rfc(20);

        let home_dir = tempfile::tempdir().unwrap();
        let codex_home = home_dir.path().join(".codex");
        let date_dir = codex_home.join("sessions/2026/10/13");
        std::fs::create_dir_all(&date_dir).unwrap();
        let rollout = date_dir.join(format!("rollout-{session_id}.jsonl"));
        std::fs::write(
            &rollout,
            format!(
                "{{\"ordinal\":0,\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session_id}\",\"session_id\":\"{session_id}\",\"cwd\":\"/w\",\"timestamp\":\"2026-10-13T00:00:00.000Z\"}}}}\n"
            ),
        )
        .unwrap();
        let append_turn = |turn: &str, input: u64, output: u64, at: &str| {
            let total = input + output;
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
        };
        append_turn("t1", 1000, 100, &at1);
        append_turn("t2", 500, 20, &at2);

        // Learn the counters/session stock the SAME rollout yields through the
        // real adapter — these are exactly the per-turn Turn snapshot and the
        // cumulative Session snapshots main's codex path wrote.
        let mut probe = CodexAdapter::new(AdapterHome {
            home: codex_home.clone(),
            cwd: "/w".into(),
            pid: None,
            launched_at: None,
            resume_session_id: None,
        });
        probe.bind_rollout(session_id, rollout.clone());
        let observed = probe.poll().unwrap();
        let stamp_ctx = StampCtx {
            instance_id: InstanceId::new(),
            host_id: remuda_protocol::HostId::new(),
            journal_id: remuda_protocol::Id::new("obj").unwrap(),
            run_id: remuda_protocol::RunId::new(),
            session_id: session_id.into(),
        };
        let mut projected: Vec<(String, Option<String>, UsageEventRow)> = Vec::new();
        for (i, obs) in observed.iter().enumerate() {
            let ObservationPayload::Usage(_) = &obs.payload else {
                continue;
            };
            let stamped = stamp(&stamp_ctx, i as u64 + 1, obs).unwrap();
            let event = serde_json::to_value(&stamped).unwrap();
            let rec = JournalRecord {
                instance_id: instance_id.into(),
                seq: i as i64 + 1,
                event_id: stamped.event_id.as_id().as_str().to_owned(),
                event,
                observed_at: at1.clone(),
            };
            let row = project_usage_event(&rec, Some(profile), Some(model)).unwrap();
            let scope = row.scope.clone();
            let turn = row.turn_id.clone();
            projected.push((scope, turn, row));
        }
        let messages: Vec<&UsageEventRow> = projected
            .iter()
            .filter(|(scope, _, _)| scope == "message")
            .map(|(_, _, row)| row)
            .collect();
        let sessions: Vec<&UsageEventRow> = projected
            .iter()
            .filter(|(scope, _, _)| scope == "session")
            .map(|(_, _, row)| row)
            .collect();
        assert_eq!(messages.len(), 2, "one Message per call");
        assert_eq!(
            sessions.len(),
            2,
            "one cumulative Session snapshot per call"
        );
        let t1_msg = messages[0];
        let t2_msg = messages[1];
        let sess1 = sessions[0];
        let sess2 = sessions[1];
        let expected_output = t1_msg.output_tokens.unwrap() + t2_msg.output_tokens.unwrap();
        let expected_input = t1_msg.input_tokens.unwrap() + t2_msg.input_tokens.unwrap();
        let expected_cost: f64 = messages
            .iter()
            .map(|row| {
                row.cost_usd
                    .as_deref()
                    .unwrap_or("0")
                    .parse::<f64>()
                    .unwrap()
            })
            .sum();

        // --- The FIELD database: origin/main's REAL schema ------------------
        let conn = Connection::open_in_memory().unwrap();
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

        // Main's journal retained the WHOLE event, including the scopeId main's
        // projection then dropped. Only payload.scopeId matters to the
        // migration, so the fixture journal rows carry just that envelope.
        conn.execute(
            "INSERT INTO journal (instance_id, seq, event_id, payload_json, observed_at)
             VALUES (?1, 1, 'je1', ?2, ?3), (?1, 2, 'je2', ?4, ?3),
                    (?1, 3, 'je3', ?5, ?6), (?1, 4, 'je4', ?7, ?6)",
            params![
                instance_id,
                json!({"kind":"usage","payload":{"scope":"turn","scopeId":"t1"}}).to_string(),
                at1,
                json!({"kind":"usage","payload":{"scope":"session","scopeId":session_id}})
                    .to_string(),
                json!({"kind":"usage","payload":{"scope":"turn","scopeId":"t2"}}).to_string(),
                at2,
                json!({"kind":"usage","payload":{"scope":"session","scopeId":session_id}})
                    .to_string(),
            ],
        )
        .unwrap();

        // Main's insert path: one Turn row per call (NO scope_id/turn_id
        // columns persisted) and one cumulative Session snapshot per call,
        // counters equal to the real per-turn totals / cumulative stock.
        let insert_main_row = |seq: i64, scope: &str, r: &UsageEventRow| {
            conn.execute(
                "INSERT INTO usage_events
                    (instance_id, seq, profile_id, model, scope, mode,
                     total_tokens, input_tokens, output_tokens,
                     cache_read_tokens, cache_write_tokens, cost_usd,
                     accounting, observed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'snapshot', ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    instance_id,
                    seq,
                    profile,
                    model,
                    scope,
                    r.total_tokens,
                    r.input_tokens,
                    r.output_tokens,
                    r.cache_read_tokens,
                    r.cache_write_tokens,
                    r.cost_usd,
                    r.accounting,
                    r.observed_at,
                ],
            )
            .unwrap();
        };
        insert_main_row(1, "turn", t1_msg);
        insert_main_row(2, "session", sess1);
        insert_main_row(3, "turn", t2_msg);
        insert_main_row(4, "session", sess2);

        // --- Upgrade: the migration under test ------------------------------
        migrate(&conn).unwrap();
        let backfilled: Vec<(String, Option<String>)> = {
            let mut stmt = conn
                .prepare("SELECT scope, scope_id FROM usage_events ORDER BY seq")
                .unwrap();
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        assert_eq!(
            backfilled,
            vec![
                ("turn".to_string(), Some("t1".to_string())),
                ("session".to_string(), Some(session_id.to_string())),
                ("turn".to_string(), Some("t2".to_string())),
                ("session".to_string(), Some(session_id.to_string())),
            ],
            "main's dropped scopeId is recovered from the journal"
        );

        // The probe held the process-wide rollout claim; release it before the
        // byte-zero replay adapters bind (exactly the restart/reclaim handoff).
        drop(probe);
        // --- Two real byte-zero replays through the PUBLIC ingest path ------
        let mut seq = 100i64;
        for replay in 1..=2 {
            let mut adapter = CodexAdapter::new(AdapterHome {
                home: codex_home.clone(),
                cwd: "/w".into(),
                pid: None,
                launched_at: None,
                resume_session_id: None,
            });
            adapter.bind_rollout(session_id, rollout.clone());
            let polled = adapter.poll().unwrap();
            for obs in polled {
                let ObservationPayload::Usage(_) = &obs.payload else {
                    continue;
                };
                let stamped = stamp(&stamp_ctx, seq as u64 + 1, &obs).unwrap();
                let event = serde_json::to_value(&stamped).unwrap();
                let rec = JournalRecord {
                    instance_id: instance_id.into(),
                    seq,
                    event_id: format!("je_replay_{replay}_{seq}"),
                    event,
                    observed_at: "2026-10-13T01:00:00.000Z".into(),
                };
                let row = project_usage_event(&rec, Some(profile), Some(model)).unwrap();
                insert_usage_event(&conn, &row).unwrap();
                seq += 1;
            }
        }

        let durable_messages: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_events
                 WHERE instance_id = ?1 AND scope = 'message'",
                [instance_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            durable_messages, 2,
            "two replays persist the same two Messages"
        );
        let eligible_turns: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM usage_events
                     WHERE instance_id = ?1 AND scope = 'turn' AND {p}",
                    p = eligible_flow_predicate("usage_events")
                ),
                [instance_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            eligible_turns, 0,
            "bucket-sum equality suppresses both Turns"
        );
        let eligible_messages: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM usage_events
                     WHERE instance_id = ?1 AND scope = 'message' AND {p}",
                    p = eligible_flow_predicate("usage_events")
                ),
                [instance_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(eligible_messages, 2, "each call's durable Message counts");

        let agg = aggregate_instance(&conn, instance_id).unwrap();
        assert_eq!(
            agg.input_tokens,
            Some(expected_input),
            "both calls' input, once"
        );
        assert_eq!(
            agg.output_tokens,
            Some(expected_output),
            "100 + 20 counted once: no Turn+Message pair, no replay duplicate"
        );
        assert!(
            (agg.cost_usd.unwrap() - expected_cost).abs() < 1e-9,
            "cost is the two Messages' priced sum once, got {} want {expected_cost}",
            agg.cost_usd.unwrap()
        );
        let supply = aggregate_supply(&conn, profile, model).unwrap();
        assert_eq!(supply.output_tokens, Some(expected_output));
        assert!((supply.cost_usd.unwrap() - expected_cost).abs() < 1e-9);

        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id,
                kind: "codex",
                spec_model: Some(model),
                effective_model: None,
                profile_id: Some(profile),
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(rollup.turns, 2, "TPM turn count: each call once");
        assert_eq!(
            rollup.session_output_tokens,
            Some(expected_output),
            "totals are the Message flow, never the Turn rows on top"
        );
        assert_eq!(
            rollup.tpm_out_60s,
            Some(expected_output),
            "native Messages inside the 60 s window count each call once"
        );
        assert_eq!(
            rollup.tpm_out_5m,
            Some((expected_output as f64 / 5.0).round() as i64),
            "5 m per-minute average likewise counts each call once"
        );
        assert_eq!(rollup.tpm_in_60s, Some(expected_input));
    }

    /// Create main's REAL `usage_events` + `journal` tables (origin/main
    /// schema: no scope_id/turn_id/metric_revision columns) in `conn`.
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

    /// Insert a Turn row EXACTLY as origin/main's codex insert path wrote it
    /// (no scope_id/turn_id persisted) plus the durable journal row whose
    /// payload retains the scopeId main dropped.
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

    /// Project one codex turn's real per-call counters through the REAL
    /// adapter, exactly as the r13 main-shape regression does: main's Turn
    /// snapshot counters equal the per-turn token_usage_record totals.
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
    fn main_shaped_identically_bucketed_retail_turn_rows_collapse_to_the_highest_seq() {
        // c-usagefu r14 item 1 (Ruling M1), regression (a): a promoted
        // instance re-tailed after a demotion/resume makes origin/main write
        // TWO Turn rows for the SAME codex turn id at different journal
        // seqs — the same call, every token bucket identical. Main persisted
        // no scope_id; the journal backfill recovers the same scopeId for
        // both, which made the new unique index creation fail and brick the
        // Hub open. The migration must instead collapse them to the HIGHEST
        // seq and count the call exactly once.
        let home = tempfile::tempdir().unwrap();
        let at = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let row = real_codex_turn_counters(home.path(), "r14m1a", "t1", 1000, 100, &at);
        let expected_output = row.output_tokens.unwrap();
        let expected_input = row.input_tokens.unwrap();

        let conn = Connection::open_in_memory().unwrap();
        create_main_shaped_usage_tables(&conn);
        // The same call projected twice — main's promoted re-tail at seq 5.
        insert_main_turn_with_journal(&conn, "ins_r14m1a", 1, "t1", &row, &at);
        insert_main_turn_with_journal(&conn, "ins_r14m1a", 5, "t1", &row, &at);

        migrate(&conn).unwrap();

        let survivors: Vec<(i64, Option<String>)> = {
            let mut stmt = conn
                .prepare("SELECT seq, scope_id FROM usage_events ORDER BY seq")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        assert_eq!(
            survivors,
            vec![(5, Some("t1".to_string()))],
            "identical buckets collapse to one survivor at the highest seq"
        );
        let agg = aggregate_instance(&conn, "ins_r14m1a").unwrap();
        assert_eq!(agg.input_tokens, Some(expected_input));
        assert_eq!(
            agg.output_tokens,
            Some(expected_output),
            "the re-tailed call is counted exactly once"
        );
        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_r14m1a",
                kind: "codex",
                spec_model: Some("gpt-5.4"),
                effective_model: None,
                profile_id: Some("pvp_r14"),
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(rollup.turns, 1, "one turn, counted once");
    }

    #[test]
    fn main_shaped_turn_rows_sharing_a_scope_id_with_different_buckets_return_to_null_scope() {
        // c-usagefu r14 item 1 (Ruling M1), regression (b): two genuinely
        // DIFFERENT result frames recover the SAME scopeId (a degenerate id,
        // both mapped to "0") while reporting different token buckets. They
        // are two calls and must NOT be merged: every row of the group has
        // its scope_id cleared back to NULL, so both stay on the legacy
        // NULL-scope path, the unique index creates, and each call counts
        // exactly once.
        let home = tempfile::tempdir().unwrap();
        let at = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let one = real_codex_turn_counters(home.path(), "r14m1b1", "ta", 1000, 100, &at);
        // A separate home/rollout for the second call: same degenerate
        // scopeId in the JOURNAL, different real counters.
        let home2 = tempfile::tempdir().unwrap();
        let two = real_codex_turn_counters(home2.path(), "r14m1b2", "tb", 2000, 200, &at);
        let expected_output = one.output_tokens.unwrap() + two.output_tokens.unwrap();
        let expected_input = one.input_tokens.unwrap() + two.input_tokens.unwrap();

        let conn = Connection::open_in_memory().unwrap();
        create_main_shaped_usage_tables(&conn);
        insert_main_turn_with_journal(&conn, "ins_r14m1b", 1, "0", &one, &at);
        insert_main_turn_with_journal(&conn, "ins_r14m1b", 5, "0", &two, &at);

        migrate(&conn).unwrap();

        let rows: Vec<(i64, Option<String>)> = {
            let mut stmt = conn
                .prepare("SELECT seq, scope_id FROM usage_events ORDER BY seq")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        assert_eq!(
            rows,
            vec![(1, None), (5, None)],
            "disagreeing buckets are never merged; both rows return to legacy NULL scope"
        );
        let agg = aggregate_instance(&conn, "ins_r14m1b").unwrap();
        assert_eq!(agg.input_tokens, Some(expected_input));
        assert_eq!(
            agg.output_tokens,
            Some(expected_output),
            "two different calls each counted once, nothing merged or doubled"
        );
        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_r14m1b",
                kind: "codex",
                spec_model: Some("gpt-5.4"),
                effective_model: None,
                profile_id: Some("pvp_r14"),
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(rollup.turns, 2, "each NULL-scope turn counts as one call");
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

    /// The usagefu MAIN-parent (cbac58e1) usage shape: every column that
    /// release had — INCLUDING scope_id — but not this delivery's
    /// source_session_id / lineage_state. A failed migration from HERE is the
    /// dangerous case: a scope_id-only current-schema check would mistake it
    /// for fully migrated (r37 item 4).
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

    /// A codex Message/Session usage frame carrying full attribution, exactly as
    /// the durable journal retains it; its projected row is written through the
    /// LEGACY insert while the migration is rolled back (14 columns only).
    #[allow(clippy::too_many_arguments)]
    fn fallback_journal_record(
        seq: i64,
        scope: &str,
        scope_id: &str,
        native_turn: Option<&str>,
        revision: i64,
        input: i64,
        output: i64,
        native_at: &str,
    ) -> JournalRecord {
        let total = input + output;
        let mut event = json!({
            "kind": "usage",
            "nativeAt": { "state": "known", "value": native_at },
            "payload": {
                "scope": scope,
                "scopeId": scope_id,
                "mode": "snapshot",
                "metricRevision": revision.to_string(),
                "contextWindow": "200000",
                "totalTokens": { "state": "known", "value": total.to_string() },
                "inputTokens": { "state": "known", "value": input.to_string() },
                "outputTokens": { "state": "known", "value": output.to_string() },
                "cacheReadTokens": { "state": "unknown", "reason": "x", "evidenceEventIds": [] },
                "cacheWriteTokens": { "state": "unknown", "reason": "x", "evidenceEventIds": [] },
                "cost": { "state": "known",
                          "value": { "amount": "0.05", "currency": "USD" } },
                "accounting": "estimated"
            }
        });
        if let Some(turn) = native_turn {
            event["source"] = json!({ "nativeTurnId": { "state": "known", "value": turn } });
        }
        JournalRecord {
            instance_id: "ins_r16item8".into(),
            seq,
            event_id: format!("je8-{seq}"),
            event,
            observed_at: "2026-10-13T00:00:09.000Z".into(),
        }
    }

    #[test]
    fn a_later_migration_reconstructs_fallback_period_codex_message_and_session_usage() {
        // c-usagefu r16 item 8: usage recorded WHILE the migration stayed rolled
        // back went through the 14-column legacy insert, so codex Message/Session
        // frames lost turn_id / native time / context window / revision even
        // though the durable journal retained them. A later successful migration
        // reconstructs those fields BEFORE consolidation so the call and its
        // attribution/tokens fold exactly once.
        let conn = Connection::open_in_memory().unwrap();
        create_main_shaped_usage_tables(&conn);
        let at = "2026-10-13T00:00:00.000Z";

        // (1) An origin/main-era Turn (seq 1), with its journal scopeId.
        let one = UsageEventRow {
            turn_id: None,
            native_context_window: None,
            instance_id: "ins_r16item8".into(),
            seq: 1,
            profile_id: None,
            model: Some("gpt-5.4".into()),
            scope: "turn".into(),
            scope_id: Some("ta".into()),
            source_session_id: None,
            mode: "snapshot".into(),
            total_tokens: Some(100),
            input_tokens: Some(60),
            output_tokens: Some(40),
            cache_read_tokens: None,
            cache_write_tokens: None,
            cost_usd: Some("0.01".into()),
            accounting: "estimated".into(),
            observed_at: at.into(),
            observed_at_source: "ingest".into(),
            metric_revision: 1,
        };
        insert_main_turn_with_journal(&conn, "ins_r16item8", 1, "ta", &one, at);

        // (2) Force the migration to fail/roll back, then record fallback usage.
        conn.execute_batch("CREATE TABLE remuda_test_force_usage_migration_failure (x);")
            .unwrap();
        assert!(migrate(&conn).is_err());
        assert!(
            !crate::store::column_exists(&conn, "usage_events", "turn_id").unwrap(),
            "the additive projection column never committed during fallback"
        );

        // A fallback codex Message (seq 2): a DIFFERENT call, full attribution.
        let msg = fallback_journal_record(
            2,
            "message",
            "mb-call",
            Some("nt-mb"),
            2,
            70,
            30,
            "2026-10-13T00:00:05.000Z",
        );
        conn.execute(
            "INSERT INTO journal (instance_id, seq, event_id, payload_json, observed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                msg.instance_id,
                msg.seq,
                msg.event_id,
                msg.event.to_string(),
                msg.observed_at
            ],
        )
        .unwrap();
        assert!(
            insert(&conn, &msg),
            "the message lands on the legacy insert"
        );

        // A fallback cumulative Session growth point (seq 3).
        let sess = fallback_journal_record(
            3,
            "session",
            "sess-1",
            None,
            1,
            200,
            100,
            "2026-10-13T00:00:06.000Z",
        );
        conn.execute(
            "INSERT INTO journal (instance_id, seq, event_id, payload_json, observed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                sess.instance_id,
                sess.seq,
                sess.event_id,
                sess.event.to_string(),
                sess.observed_at
            ],
        )
        .unwrap();
        assert!(
            insert(&conn, &sess),
            "the session point lands on the legacy insert"
        );

        // (3) Migration recovers and completes.
        conn.execute_batch("DROP TABLE remuda_test_force_usage_migration_failure;")
            .unwrap();
        migrate(&conn).unwrap();

        // The Message row is fully reconstructed from the journal.
        let (turn_id, source, revision, window) = conn
            .query_row(
                "SELECT turn_id, observed_at_source, metric_revision, native_context_window
                 FROM usage_events WHERE instance_id='ins_r16item8' AND seq=2",
                [],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            turn_id.as_deref(),
            Some("nt-mb"),
            "native turn id recovered"
        );
        assert_eq!(source, "native", "native observation time/source recovered");
        assert_eq!(revision, 2, "metric revision recovered");
        assert_eq!(window, Some(200_000), "native context window recovered");
        assert_eq!(
            conn.query_row(
                "SELECT observed_at FROM usage_events WHERE seq=2",
                [],
                |r| r.get::<_, String>(0),
            )
            .unwrap(),
            "2026-10-13T00:00:05.000Z",
            "the native call time, not the ingest time, drives rate windows"
        );
        assert_eq!(
            conn.query_row(
                "SELECT observed_at_source FROM usage_events WHERE seq=3",
                [],
                |r| r.get::<_, String>(0),
            )
            .unwrap(),
            "native",
            "the fallback session growth point is native-time stamped"
        );

        // Totals + attribution: the old Turn (100) and the fallback Message
        // (100) are two distinct calls, each counted once.
        let agg = aggregate_instance(&conn, "ins_r16item8").unwrap();
        assert!(!agg.unavailable);
        assert_eq!(
            agg.total_tokens,
            Some(200),
            "fallback Message usage is not omitted"
        );
        assert_eq!(agg.input_tokens, Some(130), "60 (turn) + 70 (message)");
        assert_eq!(agg.output_tokens, Some(70), "40 (turn) + 30 (message)");
        assert!(
            (agg.cost_usd.unwrap() - 0.06).abs() < 1e-9,
            "0.01 turn + 0.05 message attribution survives recovery"
        );

        // Rollup: both calls fold (distinct unmatched-Turn scope id + the
        // Message's native turn id), no double count; the native window resolves.
        let req = RollupRequest {
            instance_id: "ins_r16item8",
            kind: "codex",
            spec_model: Some("gpt-5.4"),
            effective_model: None,
            profile_id: None,
        };
        let rollup = rollup_instance(&conn, &req).unwrap().unwrap();
        assert_eq!(
            rollup.turns, 2,
            "old turn and fallback message each count once"
        );
        assert_eq!(
            rollup.context_window_tokens,
            Some(200_000),
            "the fallback message's native window resolves the context chip"
        );

        // Rate windows see the native-time Message; the old ingest-time Turn is
        // not native, so a threshold after it keeps only the message's flow.
        let (flow_in, flow_out) =
            sum_turn_tokens_since(&conn, "ins_r16item8", "2026-10-13T00:00:04.000Z").unwrap();
        assert_eq!(
            flow_in,
            Some(70),
            "native fallback message enters the TPM window"
        );
        assert_eq!(flow_out, Some(30));
    }

    #[test]
    fn the_usage_migration_is_idempotent_across_restart_after_restart() {
        // c-usagefu r15 item 2: after a SUCCESSFUL migration the next start
        // must not re-run the journal backfill — it would re-create the
        // scope ids the disagreeing-buckets collapse intentionally cleared to
        // NULL while the unique partial index already exists, failing the
        // migration on every restart. Migrate, restart, restart: no error, the
        // NULL scope ids and the counted totals stay identical.
        let home = tempfile::tempdir().unwrap();
        let at = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let one = real_codex_turn_counters(home.path(), "r15idem1", "ta", 1000, 100, &at);
        let home2 = tempfile::tempdir().unwrap();
        let two = real_codex_turn_counters(home2.path(), "r15idem2", "tb", 2000, 200, &at);
        let expected_output = one.output_tokens.unwrap() + two.output_tokens.unwrap();

        let conn = Connection::open_in_memory().unwrap();
        create_main_shaped_usage_tables(&conn);
        insert_main_turn_with_journal(&conn, "ins_r15idem", 1, "0", &one, &at);
        insert_main_turn_with_journal(&conn, "ins_r15idem", 5, "0", &two, &at);

        migrate(&conn).unwrap();
        let scope_ids_after_first = |conn: &Connection| -> Vec<Option<String>> {
            let mut stmt = conn
                .prepare("SELECT scope_id FROM usage_events ORDER BY seq")
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        assert_eq!(
            scope_ids_after_first(&conn),
            vec![None, None],
            "disagreeing buckets intentionally cleared to NULL by the first migration"
        );
        let first_agg = aggregate_instance(&conn, "ins_r15idem").unwrap();

        // Two full restarts: the marker skips the one-time backfill each time.
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();

        assert_eq!(
            scope_ids_after_first(&conn),
            vec![None, None],
            "the cleared scope ids are never re-created into the unique index"
        );
        let marker: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_schema_meta
                 WHERE key='scope_consolidation_v1' AND value='applied'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(marker, 1, "the completion marker is persisted");
        let agg = aggregate_instance(&conn, "ins_r15idem").unwrap();
        assert_eq!(
            (agg.events, agg.input_tokens, agg.output_tokens),
            (
                first_agg.events,
                first_agg.input_tokens,
                first_agg.output_tokens
            ),
            "counts are identical after migrate -> restart -> restart"
        );
        assert_eq!(agg.output_tokens, Some(expected_output));
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

    /// A session growth point carrying token counters AND a reported cost.
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
                insert_row(&conn, &row).unwrap();
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
        assert!(insert_row(&conn, &first_row).unwrap());
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
        let changed = insert_row(&conn, &second_row).expect("upsert succeeds");
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
        let first = insert_row(&conn, &message_row(1, "r1", 100)).unwrap();
        assert!(first, "first r1 inserted");
        assert!(insert_row(&conn, &message_row(2, "r2", 200)).unwrap());

        // Second adapter replays the same rollout (different journal seqs but
        // identical response ids): both must be ignored.
        assert!(
            !insert_row(&conn, &message_row(3, "r1", 100)).unwrap(),
            "replayed r1 is deduped by response id"
        );
        assert!(
            !insert_row(&conn, &message_row(4, "r2", 200)).unwrap(),
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
        )
        .unwrap();
        insert_row(
            &conn,
            &project(2, "session", "s1", 1, 40, 10, 100, 5, "0.10", &t1, None),
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
        )
        .unwrap();
        insert_row(
            &conn,
            &project(4, "session", "s1", 2, 100, 30, 250, 12, "0.30", &t2, None),
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
        insert_row(&conn, &project(1, "message", "r1")).unwrap();
        insert_row(&conn, &project(2, "turn", "t1")).unwrap();

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
        )
        .unwrap();
        seq += 1;
        insert_row(&conn, &project(seq, "turn", "t1", Some("t1"), Some("0.10"))).unwrap();
        seq += 1;
        // Session cumulative point priced $0.10.
        insert_usage_event(&conn, &s1(seq, 1, Some("0.10"))).unwrap();
        seq += 1;
        // Turn 2 unpriced: Message + Turn both unknown cost; session NULL.
        insert_row(&conn, &project(seq, "message", "r2", Some("t2"), None)).unwrap();
        seq += 1;
        insert_row(&conn, &project(seq, "turn", "t2", Some("t2"), None)).unwrap();
        seq += 1;
        insert_usage_event(&conn, &s1(seq, 2, None)).unwrap();
        seq += 1;
        // Turn 3 priced $0.20: Message + Turn + session.
        insert_row(
            &conn,
            &project(seq, "message", "r3", Some("t3"), Some("0.20")),
        )
        .unwrap();
        seq += 1;
        insert_row(&conn, &project(seq, "turn", "t3", Some("t3"), Some("0.20"))).unwrap();
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

    /// A result-frame session point: no token buckets (the SDK result omits
    /// per-call counters), just the reported cumulative cost.
    fn tokenless_session_record(
        seq: i64,
        scope_id: &str,
        revision: i64,
        cost: Option<&str>,
    ) -> JournalRecord {
        tokenless_session_record_accounting(seq, scope_id, revision, cost, "estimated")
    }

    /// r39 items 2/3: session carrier with explicit accounting so the REPORTED
    /// native-bill path is exercised (drivers mark result costs reported).
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

    /// r39 item 2 (AA2): a REPORTED cumulative bill is authoritative only for
    /// its own instance. Instance A reports $0.01; instance B has an uncovered
    /// per-call ESTIMATE of $0.02 — the provider total is $0.03 (and a $0.02
    /// budget must Stop), not just A's $0.01.

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

    /// r39 item 3 (AA2): a legacy NULL-scope REPORTED bill is suppressed once
    /// scoped session evidence exists for the instance — same legacy-vs-scoped
    /// selection as token stock. NULL $0.40 then scoped $0.50 bills $0.50.
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
        )
        .unwrap();
        // New Message r2 for a turn with NO Turn row (eligible flow).
        insert_row(
            &conn,
            &project(2, "message", "resp2", Some("t2"), 50, Some("0.05")),
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

    /// Token-less session growth point with an explicit instance id.
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

    /// r39: token-less session carrier for an arbitrary instance with explicit
    /// accounting (REPORTED native bill vs ESTIMATED priced stock).
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

    /// A priced per-call (turn) row for an arbitrary instance.
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

    /// RFC3339 stamp `seconds` before now, same fixed-width millisecond
    /// shape the store writes (so the SQL window predicates compare
    /// lexicographically).
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

    /// c-ctxusage r17 item 5: the per-lineage winner is chosen across ALL
    /// native rows BEFORE the rate window is applied. A superseded in-window
    /// lineage must not be counted when the winning lineage's own timestamp
    /// is outside the window, and a winner whose timestamp IS in the window is
    /// counted despite an older lineage row lying outside it.
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

    /// c-ctxusage r22 item 1: rank EVERY lineage row (native AND ingest
    /// provenance) before native-time eligibility. A newer accepted winner
    /// carrying an INGEST timestamp (a replay landed at journal seq) must
    /// supersede an older native-time lineage; that older in-window native row
    /// must not be counted, and the ingest-time winner itself is excluded
    /// (rate windows trust native time only).
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

    /// c-ctxusage r22 item 2: cumulative SESSION snapshots fold to the LATEST
    /// accepted stock — 100 then 200 is 200, one event, never 300.
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

    /// c-ctxusage r22 item 2: when an emitter (Codex) sends BOTH per-turn flows
    /// and a cumulative session snapshot that already contains them, the
    /// aggregate counts the STOCK once and drops the covered turns; turns of a
    /// session with no stock are still summed.
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

    /// U1 (c-ctxusage r17 item 6), turn context basket: the newest call's
    /// `input + cache_read + cache_write` is Known only when ALL THREE buckets
    /// are reported. Input 100 with an unknown cache is Unknown context, never
    /// a known 100.
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

    /// U1, stock-only context fallback: a cumulative stock whose latest point
    /// reports input but not a cache bucket yields Unknown context rather than
    /// summing only the known baskets.
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

    /// U1, cross-session stock addition: the total across sessions is Known
    /// only when EVERY session's latest point reports the bucket. One session
    /// with unknown input makes the folded input Unknown even though another
    /// session reports 100; independently-known output still sums. Two fully
    /// known sessions (the control instance) sum normally.
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

    /// U1, session rate folding: a rate-window bucket is Unknown when ANY
    /// in-window increment omits it, even alongside a known increment. Two
    /// native intervals land inside 60 s — input +50 then an interval whose
    /// input is unknown (output +5 both times) — so tpm_in is Unknown while
    /// tpm_out is the known sum 10.
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

    /// Codex/Grok-shaped observations report only some channels. Missing
    /// counters stay NULL end to end and roll up to `None` — never 0 — and
    /// when nothing composing the next request's context was reported, the
    /// context figure (and therefore the ring percentage) is unknown too.
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

    /// Build a usage journal record exactly like the wire: counters are
    /// `Knowledge<U64>` objects but `metricRevision` is the BARE protocol `U64`
    /// scalar (a decimal string).
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

    /// Qualify a turn record with its native session id, the production Codex
    /// shape that lets a session stock cover the turn (c-ctxusage r28 item 1).
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

    /// c-ctxusage r3 items 1, 4, 5, 7: drive the REAL TranscriptMapper, project
    /// every emitted usage observation through the real store, and assert the
    /// durable rows hold the FINAL counters, merge with a round-one (plain id)
    /// row, and keep a historical native time out of the rate windows.
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

    /// r4 item 3: a restarted adapter replays historical cumulative stocks
    /// with fresh journal seqs. Either arrival order — historical first then
    /// current, or current first then historical — must leave the CURRENT
    /// (larger) stock durable, never the replayed smaller one.
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

    /// r4 item 4: correcting an OLDER message's provisional counters at a high
    /// ingest seq must not make it the current context — "current" follows the
    /// native time, with a deterministic tiebreak.
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

    /// c-ctxusage r4 item 5 / r14 items 3-4: a NEWER source position with
    /// IDENTICAL counters but the correct historical native time repairs an
    /// ingest-fallback observed_at — lastTurnAt becomes historical and the
    /// rate windows empty — with no counter change. An equal-position replay
    /// would be frozen; the clock follows only a strictly newer snapshot.
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

    /// c-ctxusage r14 items 3/4: an equal-position replay changes NOTHING, even
    /// a native one carrying the exact fallback timestamp — provenance is part
    /// of the turn clock and an equal source position never rewrites it. A
    /// STRICTLY NEWER native position repairs the stored ingest-fallback
    /// provenance, after which another identical newer frame is a no-op.
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
    /// c-ctxusage r14 item 3: the turn authority is a watermark that advances
    /// on a STRICTLY NEWER position even when the counters are unchanged.
    /// Sequence from the ruling: rev 1 / output 20, then rev 3 / output 20
    /// (identical), then a stale rev 2 carrying a decrease to 15 — the row
    /// must stay at rev 3 / 20. Before the fix the identical newer snapshot
    /// was dropped ("nothing changed"), leaving the watermark at 1, so the
    /// later rev 2 was accepted as newer and rolled output back to 15.
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

    /// c-ctxusage r15 item 2: an EQUAL source position changes NOTHING — not a
    /// counter growth, not a bucket fill, not the seq. The watermark is a
    /// strict > authority: two frames at the same position are the same source
    /// record however often it is re-delivered, even when a later delivery
    /// carries a bigger bucket or a previously omitted one.
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

    /// c-ctxusage r15 item 3: a COST-ONLY frame is a billing correction on a
    /// separate channel but it still rides the strict source-position gate.
    /// A stale cost-only position changes nothing; a strictly newer cost-only
    /// position corrects the bill AND advances the authority watermark (so a
    /// later counter snapshot at an older position is rejected), while
    /// counters and the turn clock are kept exactly as stored.
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

    /// c-ctxusage r15 item 4 (refined by r16 item 3 / T1'): existing rows
    /// migrate into the UNQUALIFIED lineage (`source_session_id IS NULL`),
    /// and two unqualified frames keep the numeric watermark. A qualified
    /// frame over the legacy NULL lineage is an independent authority and is
    /// not numerically frozen — it gets its OWN revision-replaced row;
    /// accepting it never erases the NULL lineage's mark or identity.
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

    /// Build a qualified turn record in a specific native-session lineage.
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

    /// c-ctxusage r16 item 3 (T1'), regression 1: alpha at position 3, then a
    /// beta-lineage frame at position 1, then alpha at position 2 — the beta
    /// acceptance must not erase alpha's mark, so the older alpha frame is
    /// rejected within its own lineage.
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

    /// r37 item 1 (HIGH): the legacy duplicate-turn reconciliation is a
    /// ONE-TIME fixup for the pre-lineage schema. On an already-committed
    /// database it must not run again, and even on the one upgrade where it
    /// runs it must group by the full lineage key: its old grouping ignored
    /// `source_session_id`, so a Hub restart deleted a retained per-lineage
    /// watermark (the other session's non-max-seq row), after which a stale
    /// replay of that lineage found no own row, inserted fresh and won.
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

    /// r37 item 2: a REAL upgrade from the pre-`lineage_state` schema tags
    /// every retained row 'legacy', including turns that are lineage-QUALIFIED
    /// by a retained `source_session_id`. Such a row belongs to the proven-flow
    /// branch ALONE; it must never also match the unqualified legacy-open /
    /// legacy-flow branches (their `UNION ALL` then counts it twice and the
    /// flow branch stands unrelated stock down).
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

    /// r38 item 2: a final snapshot written through the 14-column fallback
    /// insert while an upgrade was rolled back, then recovered from the journal
    /// on the successful retry, is a LATER REVISION of one proven turn — not a
    /// second degenerate call. The disagreeing-key reconciliation belongs to
    /// the historical unqualified producer; recovered snapshots with proven
    /// common lineage keep their scope/lineage and resolve to the fold winner.
    #[test]
    fn recovered_lineage_snapshots_resolve_through_revision_not_degenerate_ids() {
        let conn = Connection::open_in_memory().unwrap();
        // r28 shape, already migrated once: the established turn at rev 100.
        create_r28_usage_table_with_turn_index(&conn);
        create_journal_table(&conn);
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, profile_id, model, scope, scope_id, mode,
                 metric_revision, source_session_id, total_tokens, input_tokens,
                 output_tokens, cache_read_tokens, cache_write_tokens,
                 accounting, observed_at, observed_at_source)
             VALUES ('ins_r38i2', 1, 'pvp', 'm', 'turn', 'msg-T', 'snapshot',
                 100, 'sess-S', 100, 100, 0, 0, 0,
                 'estimated', '2026-10-01T00:00:00.000Z', 'native')",
            [],
        )
        .unwrap();
        // While the upgrade stayed rolled back, the final rev-101 snapshot was
        // recorded through the 14-column legacy path: identity columns NULL.
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, profile_id, model, scope, mode, total_tokens,
                 input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                 accounting, observed_at)
             VALUES ('ins_r38i2', 2, 'pvp', 'm', 'turn', 'snapshot', 150,
                 150, 0, 0, 0,
                 'estimated', '2026-10-01T00:00:02.000Z')",
            [],
        )
        .unwrap();
        // The durable journal retains BOTH frames: scopeId T and confirmed
        // native session S, at revisions 100 and 101.
        let journal_turn = |_seq: i64, revision: i64, input: i64, stamp: &str| {
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
            (2, 101, 150, "2026-10-01T00:00:02.000Z"),
        ] {
            conn.execute(
                "INSERT INTO journal (instance_id, seq, event_id, payload_json, observed_at)
                 VALUES ('ins_r38i2', ?1, ?2, ?3, ?4)",
                params![
                    seq,
                    format!("je{seq}"),
                    journal_turn(seq, rev, input, stamp).to_string(),
                    stamp
                ],
            )
            .unwrap();
        }

        // Successful retry of the delivery migration.
        migrate(&conn).unwrap();

        // One physical turn row survives, and it is the rev-101 winner with
        // its proven scope and lineage intact.
        let (count, scope, sid, revision, input): (i64, Option<String>, Option<String>, i64, i64) =
            conn.query_row(
                "SELECT COUNT(*), MIN(scope_id), MIN(source_session_id),
                        MAX(metric_revision), MAX(input_tokens)
                 FROM usage_events WHERE instance_id='ins_r38i2' AND scope='turn'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(count, 1, "the two snapshots are one turn, not two calls");
        assert_eq!(scope.as_deref(), Some("msg-T"), "proven scope is retained");
        assert_eq!(sid.as_deref(), Some("sess-S"), "proven lineage is retained");
        assert_eq!(revision, 101, "the final revision survives");
        assert_eq!(input, 150);

        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_r38i2",
                kind: "claude",
                spec_model: Some("m"),
                effective_model: None,
                profile_id: Some("pvp"),
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(rollup.turns, 1);
        assert_eq!(
            rollup.session_input_tokens,
            Some(150),
            "rev 101 wins, not 250"
        );
    }

    /// r39 item 1 (AA1), stale replay: recovery collapse must use the LIVE
    /// revision authority, never "highest ingestion seq wins". An established
    /// r28 rev100/100 row survives a later fallback-seq rev99/50 historical
    /// replay even though the replay lands at seq 2.
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

    /// r39 item 1 (AA1), partial newer frame: the winning higher-revision
    /// snapshot may omit buckets the older row knew; collapse must merge them
    /// exactly like the live upsert (incoming reported wins, gaps retained),
    /// never retain one raw snapshot wholesale.
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

    /// r40 item 2 (AB1), cost-only recovery equals the live cost-only channel:
    /// an established rev100/t0/input100/total150 frame (other buckets still
    /// Unknown) is followed through the fallback path by a rev101 cost-only
    /// correction at native t1. Recovery must carry EVERY counter (including a
    /// total the incomplete merged basket cannot re-derive) and the native
    /// clock forward, advancing only revision, sequence and bill — exactly what
    /// the live upsert does. The test also asserts instance/supply parity on
    /// the retained totals and bill.
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

    /// r37 item 3: on an upgrade from the schema that ALREADY carried
    /// observed_at_source (c-ctxusage r28), an explicit 'ingest' provenance is
    /// retained fact: the point's timestamp is the ingest time of a
    /// timestamp-free historical replay, not native evidence. The blanket
    /// 'legacy' migration tag must not promote it to trusted time — otherwise
    /// the replay becomes a false live throughput increment. Only rows from a
    /// shape that PRE-DATED the source column (no provenance existed) keep the
    /// c-usagefu r8 item 5 compatibility trust.
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

    /// r37 item 5: the durable-Message coverage proof compares its bucket sums
    /// against the FOLD's winning turn. With per-lineage rows for one turn, the
    /// coverage scalar subqueries used to re-read raw Turn rows without any
    /// ordering, so they could match the LOSING lineage's smaller buckets and
    /// suppress the actual (newer, larger) winner in favour of its Messages.
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

    /// r37 item 6: the has_native_flow ROUTE probe scans winning turns across
    /// EVERY instance. A native uncovered turn belonging to instance B must not
    /// route instance A down the turn-rate path; A emits only cumulative
    /// session stocks and must keep deriving its rate from stock increments
    /// (100 -> 150 = 50 inside the window), not get Unknown.
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

    /// r38 item 1: reopening the Hub must never lower an established durable
    /// Message revision watermark. `upsert_message_row` keeps the ORIGINAL
    /// stored sequence when it revises a message, so the retained journal at
    /// that sequence still carries the revision-1 payload; a restart whose
    /// fallback reconstruction re-reads it verbatim would downgrade revision 2
    /// to 1 and let a stale replay overwrite the newer counters.
    #[test]
    fn reopen_never_lowers_a_durable_message_revision_then_a_stale_replay_loses() {
        let marker = remuda_driver::adapters::codex_adapter::CODEX_DURABLE_RESPONSE_SCOPE_PREFIX;
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        create_journal_table(&conn);

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

    /// STRUCTURAL ZERO ("this model/frame has no such cache tier"), not an
    /// omitted value. The merge's context basket demanded a value in EVERY
    /// bucket, so a real Claude usage frame with known input, known cache read
    /// zero and `cacheWriteTokens: not-applicable` reported contextUsedTokens
    /// null (the supply r2c integration shape; passes on cbac58e1, fails on the
    /// post-merge tip). A genuinely OMITTED bucket still leaves the basket
    /// Unknown (U1).
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

    /// c-ctxusage r16 item 3 (T1'), regression 2: a qualified alpha frame at
    /// position 3 followed by an UNQUALIFIED frame at position 1 — the
    /// unqualified frame lands in the NULL lineage and never rewrites the
    /// qualified row's identity.
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

    /// c-ctxusage r16 item 3 (T1'), regression 3: a high-position
    /// UNQUALIFIED frame never blocks later valid frames of a qualified
    /// lineage that starts its record positions over from 1 (a resumed /
    /// re-bound session).
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

    /// r4 item 7b: the REAL 2.1.289 fixture replayed by a fresh mapper into a
    /// real migrated store TWICE must converge durably: the same 24 turn rows
    /// with the same counters after each pass (the second pass is a
    /// re-hydration — it projects and re-inserts, but nothing changes).
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

    /// Durable counters per message id: (input, output, cacheRead, cacheWrite).
    type PerScopeCounters = BTreeMap<String, (i64, i64, i64, i64)>;

    /// Independently parse the raw 2.1.289 fixtures: last non-sidechain
    /// assistant record per message id, with the 5m+1h cache-write split —
    /// exactly the source of truth the driver test uses, duplicated here so the
    /// hub's durable rows are compared against the raw file, not the mapper.
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

    /// Create the real store's `journal` table shape (r9 item 5 tests the
    /// migration-time backfill against retained journal payloads).
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

    /// Persist a journal record exactly like the real store append path.
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

    /// r5 item 3A: codex session s1 then s2 — summed current stocks, boundary
    /// is not a reset increment.
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

    /// r8 item 8 (formerly r5 item 3B): a legacy NULL-scope cumulative stock
    /// carries no session identity, so it must NOT floor scoped points — even
    /// the instance's first one. A byte-0 restart replay is simply an early
    /// per-scope growth point; the rollup's current stock follows native time,
    /// so an older historical point never lowers the current totals.
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

    /// r8 item 8: the legacy NULL-scope floor does not apply WITHIN a scope id
    /// chain either — only same-scope-id points floor each other. c-ctxusage
    /// r9 item 6 adds legacy NULL rows to both sides of the rule: a PROVEN
    /// legacy row (retained journal payload with scopeId) backfills into its
    /// session and floors that chain; an UNPROVEN legacy row stays NULL and
    /// floors nothing.
    #[test]
    fn scoped_points_are_floored_only_by_their_own_scope_id() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        create_journal_table(&conn);
        // Proven legacy NULL row for s1 (1000/100), exactly as an upgraded
        // database carries it, plus the matching retained journal payload.
        assert!(
            insert_usage_event(
                &conn,
                &legacy_session_row(1, "ins_8", 1000, 100, "2020-01-01T00:00:00.000Z")
            )
            .unwrap()
        );
        let proven = scoped_record(
            1,
            "ins_8",
            "session",
            Some("s1"),
            1,
            1000,
            100,
            0,
            0,
            Some("2020-01-01T00:00:00.000Z"),
        );
        insert_journal_raw(&conn, &proven);
        // An UNPROVEN legacy row with a much larger stock: no journal proves
        // its identity, so it must never floor any scoped session.
        assert!(
            insert_usage_event(
                &conn,
                &legacy_session_row(2, "ins_8", 9000, 900, "2020-01-02T00:00:00.000Z")
            )
            .unwrap()
        );
        // Re-run the migration: the proven legacy row must backfill to s1.
        migrate(&conn).unwrap();
        assert_eq!(
            scope_id_at(&conn, 1),
            Some("s1".to_string()),
            "the retained journal payload proves the legacy row's session"
        );
        assert_eq!(
            scope_id_at(&conn, 2),
            None,
            "an unproven legacy row must stay NULL-scope"
        );
        // Same session, backwards: frozen by the BACKFILLED s1 floor.
        assert!(
            !insert(
                &conn,
                &scoped_record(3, "ins_8", "session", Some("s1"), 1, 800, 80, 0, 0, None)
            ),
            "a backwards point within the backfilled scope id is frozen"
        );
        // Different session with a smaller stock: counted despite the 9000
        // unproven legacy row.
        assert!(
            insert(
                &conn,
                &scoped_record(4, "ins_8", "session", Some("s2"), 1, 50, 5, 0, 0, None)
            ),
            "neither s1's stock nor the unproven legacy row may floor s2"
        );
        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_8",
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
            Some(1050),
            "current stocks sum per session: 1000 (backfilled s1) + 50 (s2); \
             the unproven 9000 legacy row contributes nothing"
        );
    }

    /// c-ctxusage r9 item 5: a legacy NULL-scope stock (1000/100 for s1) used
    /// to disappear from the totals once a scoped s2 (100/10) arrived. With
    /// the retained journal payload proving s1, the migration backfills the
    /// identity BEFORE floors/aggregation: the smaller new session is
    /// accepted and the totals are 1100/110.
    #[test]
    fn journal_backfilled_legacy_scope_survives_alongside_a_scoped_session() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        create_journal_table(&conn);
        // Old-schema usage row: NULL scope_id.
        assert!(
            insert_usage_event(
                &conn,
                &legacy_session_row(1, "ins_5b", 1000, 100, "2020-01-01T00:00:00.000Z")
            )
            .unwrap()
        );
        // The journal still holds the original event with payload.scopeId.
        let proven = scoped_record(
            1,
            "ins_5b",
            "session",
            Some("s1"),
            1,
            1000,
            100,
            0,
            0,
            Some("2020-01-01T00:00:00.000Z"),
        );
        insert_journal_raw(&conn, &proven);
        migrate(&conn).unwrap();
        assert_eq!(scope_id_at(&conn, 1), Some("s1".to_string()));

        // A smaller NEW scoped session is still accepted.
        assert!(
            insert(
                &conn,
                &scoped_record(10, "ins_5b", "session", Some("s2"), 1, 100, 10, 0, 0, None)
            ),
            "the new smaller session s2 must be accepted"
        );
        // The discriminated case from the review: previously the legacy s1
        // stock vanished the instant any scoped row existed, leaving only
        // s2's 100/10. The backfilled identity keeps it in the total:
        // 1000 + 100 = 1100 / 110.
        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_5b",
                kind: "codex",
                spec_model: Some("codex"),
                effective_model: None,
                profile_id: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(rollup.session_input_tokens, Some(1100));
        assert_eq!(rollup.session_output_tokens, Some(110));

        // s1 later grows: the backfilled legacy row is its floor chain, so
        // 1200 is accepted ON TOP of the proven 1000 stock.
        assert!(insert(
            &conn,
            &scoped_record(
                11,
                "ins_5b",
                "session",
                Some("s1"),
                2,
                1200,
                120,
                0,
                0,
                None
            )
        ));

        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_5b",
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
            Some(1300),
            "1200 (s1) + 100 (s2)"
        );
        assert_eq!(rollup.session_output_tokens, Some(130));
    }

    /// c-ctxusage r9 item 5 negative case: a legacy NULL row whose journal
    /// payload carries no scopeId stays unproven and is not stamped.
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

    /// c-ctxusage r10 item 2: a database still carrying the OBSOLETE shared
    /// unique index `usage_events_scope_dedupe` (last opened by RC1-era code),
    /// with several NULL-scope legacy rows whose retained journal payloads
    /// prove ONE session — including an identical-counters duplicate and a row
    /// matching an already-scoped point — must open. The old migration stamped
    /// scope ids while the obsolete index was live and failed with a UNIQUE
    /// violation on EVERY Hub startup; the fixed migration drops first,
    /// backfills, collapses duplicates, creates the new index atomically, and
    /// is idempotent.
    #[test]
    fn old_schema_with_obsolete_index_backfills_one_proven_session_and_opens() {
        let conn = Connection::open_in_memory().unwrap();
        // The RC1-era table: no cache_* / metric_revision / source columns.
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
        let insert_legacy = |seq: i64, input: i64, output: i64, at: &str| {
            conn.execute(
                "INSERT INTO usage_events
                    (instance_id, seq, scope, scope_id, mode, total_tokens,
                     input_tokens, output_tokens, accounting, observed_at)
                 VALUES ('ins_r10', ?1, 'session', NULL, 'snapshot',
                         ?2, ?3, ?4, 'estimated', ?5)",
                params![seq, input + output, input, output, at],
            )
            .unwrap();
        };
        // Three NULL rows proving s1: 100/10, an IDENTICAL 100/10 replay, and
        // 150/15 growth.
        insert_legacy(1, 100, 10, "2020-01-01T00:00:00.000Z");
        insert_legacy(2, 100, 10, "2020-01-01T00:00:01.000Z");
        insert_legacy(3, 150, 15, "2020-01-01T00:00:02.000Z");
        // A fourth NULL row proving s1 whose 200/20 stock MATCHES an existing
        // already-scoped s1 point written after the upgrade.
        insert_legacy(4, 200, 20, "2020-01-01T00:00:04.000Z");
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, scope, scope_id, mode, total_tokens,
                 input_tokens, output_tokens, accounting, observed_at)
             VALUES ('ins_r10', 10, 'session', 's1', 'snapshot',
                     220, 200, 20, 'estimated', '2020-01-01T00:00:05.000Z')",
            [],
        )
        .unwrap();
        create_journal_table(&conn);
        // Every legacy row's retained journal payload proves session s1.
        insert_journal_raw(
            &conn,
            &scoped_record(1, "ins_r10", "session", Some("s1"), 1, 100, 10, 0, 0, None),
        );
        insert_journal_raw(
            &conn,
            &scoped_record(2, "ins_r10", "session", Some("s1"), 1, 100, 10, 0, 0, None),
        );
        insert_journal_raw(
            &conn,
            &scoped_record(3, "ins_r10", "session", Some("s1"), 1, 150, 15, 0, 0, None),
        );
        insert_journal_raw(
            &conn,
            &scoped_record(4, "ins_r10", "session", Some("s1"), 1, 200, 20, 0, 0, None),
        );

        // The migration itself must succeed — it runs in Store::open, so an
        // error here is the "Hub DB cannot open on any retry" failure.
        migrate(&conn).unwrap();

        // The obsolete index is gone; the scope-specific turn index exists.
        let old_index: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'index' AND name = 'usage_events_scope_dedupe'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_index, 0, "the obsolete shared index was not dropped");
        let new_index: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'index' AND name = 'usage_events_turn_dedupe'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(new_index, 1);

        // All rows are stamped s1; the identical 100/10 replay and the
        // matching 200/20 point collapsed: 100/10, 150/15, 200/20 remain.
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_events
                 WHERE instance_id = 'ins_r10' AND scope = 'session'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 3, "identical proven points must be consolidated");
        let null_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_events
                 WHERE instance_id = 'ins_r10' AND scope_id IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(null_rows, 0);
        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: "ins_r10",
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
            "current stock is the latest proven growth point, not a doubled sum"
        );
        assert_eq!(rollup.session_output_tokens, Some(20));

        // The migration is idempotent: a second open runs it unchanged.
        migrate(&conn).unwrap();
        let rows_again: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_events
                 WHERE instance_id = 'ins_r10' AND scope = 'session'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows_again, 3);
    }

    /// c-ctxusage r11 item 1: old-schema NULL rows can land OUT of cumulative
    /// order under the old observed-at acceptance — the high-water 200/20 row
    /// is inserted first and a stale historical replay 100/10 later, both
    /// proven by the retained journal to belong to session s1. Exact-counter
    /// consolidation keeps both, so raw rowid order would present the stale
    /// 100/10 as the current stock while the retained 200/20 still floors new
    /// points. The migration replays the cumulative-growth acceptance rule in
    /// rowid order and drops the point that rule would have rejected, leaving
    /// one coherent accepted-history chain.
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

    /// r8 item 8 regression: the legacy NULL-scope row is a different,
    /// unidentifiable session; the FIRST post-upgrade scoped session with a
    /// smaller stock is still counted, while each scope id keeps its own
    /// monotonic floor.
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

    /// r5 item 7: a bulk codex replay with NO native timestamps (ingest
    /// fallback rows) must not be read as current throughput — an
    /// hours-old rollout bound at t=now stays out of the 60 s window and has
    /// no lastTurnAt; a later turn carrying a native timestamp then enters
    /// the window normally.
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

    /// r6 item 2: a LIVE claude-print/sdk result row is stamped with its
    /// receipt time as `nativeAt`, so it enters the rate windows and sets
    /// lastTurnAt even though no historical timestamp exists; a BULK replay
    /// carrying an old native timestamp still counts totals but stays out of
    /// the current windows. The old `observed_at_source='native'` filter
    /// silently dropped every live print/sdk row because the mapper left
    /// native_at Unknown (source 'ingest').
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

    /// c-ctxusage r8 item 9: the Codex adapter emits BOTH a Turn snapshot
    /// (native time) and a Session snapshot (no nativeAt → ingest fallback)
    /// per turn end. A timestamp-free historical Session replay must not
    /// restore the ingest time as lastTurnAt or leak into session rates;
    /// trustworthy timing comes only from NATIVE timestamps.
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

    /// c-ctxusage r9 item 3: ingest-stamped replay points still walk the
    /// cumulative chain as baselines; only increment EMISSION is native
    /// gated. The real Codex adapter emits a native-timed Turn snapshot
    /// BESIDE every Session snapshot (codex_adapter end_turn), so the test
    /// includes the Turn row too and proves it never enters the session
    /// stock diff.
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

    /// RFC3339 UTC millisecond stamp `delta` seconds from now (negative for
    /// the past, positive for the future), byte-comparable with the stamps
    /// `Store::append_journal` writes.
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

    /// c-ctxusage r10 item 3: a buffered NATIVE point (native ts 12:04:50,
    /// accepted last) must diff against the last ACCEPTED predecessor (the
    /// 50 accepted at ingest 12:05:01), not the point with the adjacent
    /// native time, and the current stock is the last accepted 150 — never
    /// the newest-clock ingest row 50.
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

    /// c-ctxusage r10 item 4: an UNKNOWN predecessor counter is not zero.
    /// input unknown→150 leaves the interval unknown; output 10→15 is 5.
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

    /// A direct turn row builder for the U1 aggregate tests.
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

    /// U1 (c-ctxusage r16 item 4): folding two turns where the SECOND omits
    /// input must make the aggregate input Unknown (the first turn's number
    /// is not the complete sum); reported-by-both output stays Known.
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

    /// U1 (c-ctxusage r16 item 4): an unpriced turn makes the aggregate bill
    /// Unknown (never a COALESCE 0.0), and an Unknown cost yields budget
    /// status Unknown — never Ok — at every cap.
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

    /// U1 (c-ctxusage r16 item 4): the supply aggregate (profile, model) is
    /// Unknown on any unpriced contributing row just like the instance one.
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

    /// c-ctxusage r18 item 2: supply totals rank scoped turns across ALL
    /// models/profiles before filtering. One logical turn (same scope id)
    /// accepted under profile/model A is SUPERSEDED by a newer accepted frame
    /// under B; A's supply must not resurrect the superseded row — zero
    /// events for A — while B's supply owns the winning row.
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

    /// c-ctxusage r26 item 2: NULL-scope legacy coverage is scoped PER
    /// INSTANCE. A legacy turn in instance A ($1) and a legacy cumulative
    /// session row in instance B ($2) are unrelated even though neither could
    /// be attributed a native session; B's session must not suppress A's turn
    /// in the shared (profile, model) supply.
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

    /// c-ctxusage r26 item 2: a scoped session stock only covers turns of the
    /// SAME instance sharing its native session id. A same-named session in a
    /// different instance must not suppress another instance's turn.
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

    /// Create the exact origin/main usage_events schema (no scope_id /
    /// source_session_id / observed_at_source / metric_revision columns).
    fn create_main_usage_table(conn: &Connection) {
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
                cache_read_tokens INTEGER,
                cache_write_tokens INTEGER,
                cost_usd TEXT,
                accounting TEXT NOT NULL DEFAULT 'estimated',
                observed_at TEXT NOT NULL,
                PRIMARY KEY (instance_id, seq)
             );
             CREATE INDEX usage_events_profile_model
                 ON usage_events(profile_id, model);
             CREATE INDEX usage_events_instance
                 ON usage_events(instance_id);",
        )
        .unwrap();
    }

    /// Insert one main-schema usage row (identity columns do not exist yet).
    fn insert_main_usage_row(
        conn: &Connection,
        instance: &str,
        seq: i64,
        scope: &str,
        input: i64,
        output: i64,
        at: &str,
    ) {
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, profile_id, model, scope, mode,
                 total_tokens, input_tokens, output_tokens, cache_read_tokens,
                 cache_write_tokens, cost_usd, accounting, observed_at)
             VALUES (?1, ?2, 'pvp_r29', 'm-r29', ?3, 'snapshot',
                     ?4, ?5, ?6, 0, 0, NULL, 'estimated', ?7)",
            params![instance, seq, scope, input + output, input, output, at],
        )
        .unwrap();
    }

    /// A retained usage observation with a stamped source envelope.
    fn journal_usage_with_envelope(
        seq: i64,
        instance: &str,
        scope: &str,
        scope_id: Option<&str>,
        at: &str,
        session_state: &str,
    ) -> JournalRecord {
        let mut rec = scoped_record(seq, instance, scope, scope_id, 1, 100, 10, 0, 0, Some(at));
        rec.event["source"] = json!({
            "nativeSessionId": match session_state {
                "known" => json!({"state": "known", "value": "sess-codex"}),
                _ => json!({"state": "unknown",
                            "reason": "journal-evidence-missing",
                            "evidenceEventIds": []}),
            },
            "nativeTurnId": if scope == "turn" {
                json!({"state": "known", "value": scope_id.unwrap()})
            } else {
                json!({"state": "unknown",
                        "reason": "not-emitted",
                        "evidenceEventIds": []})
            }
        });
        rec
    }

    /// c-ctxusage r29 item 1: an origin/main database (no identity columns)
    /// with a Codex turn 100/10 and its cumulative stock 100/10. (a) the
    /// retained journal proves the turn's session via the envelope stamp ->
    /// backfilled lineage covers it, total stays 100/10; (b) lineage
    /// unprovable -> the turn is ambiguous: totals Unknown, never 200/20 and
    /// never silently dropped.
    #[test]
    fn upgraded_codex_turn_and_stock_backfill_or_become_unknown() {
        for proven in [true, false] {
            let conn = Connection::open_in_memory().unwrap();
            create_main_usage_table(&conn);
            create_journal_table(&conn);
            let instance = "ins_r29up";
            let at = "2020-01-01T00:00:00.000Z";
            // Main producer emits BOTH the per-turn flow and the stock.
            insert_main_usage_row(&conn, instance, 1, "turn", 100, 10, at);
            insert_main_usage_row(&conn, instance, 2, "session", 100, 10, at);
            // The retained usage observations: the stock is always proven;
            // the turn's envelope proves its session only in case (a).
            insert_journal_raw(
                &conn,
                &journal_usage_with_envelope(
                    1,
                    instance,
                    "turn",
                    Some("turn-1"),
                    at,
                    if proven { "known" } else { "unknown" },
                ),
            );
            insert_journal_raw(
                &conn,
                &journal_usage_with_envelope(
                    2,
                    instance,
                    "session",
                    Some("sess-codex"),
                    at,
                    "known",
                ),
            );

            migrate(&conn).unwrap();

            // Identity/lineage backfill outcomes.
            let (turn_scope, lineage): (Option<String>, Option<String>) = conn
                .query_row(
                    "SELECT scope_id, source_session_id FROM usage_events
                     WHERE instance_id = ?1 AND scope='turn'",
                    params![instance],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(
                turn_scope.as_deref(),
                Some("turn-1"),
                "turn identity restored"
            );
            assert_eq!(
                lineage.as_deref(),
                if proven { Some("sess-codex") } else { None },
                "lineage is stamped only from proof"
            );

            let rollup = rollup_instance(
                &conn,
                &RollupRequest {
                    instance_id: instance,
                    kind: "codex",
                    spec_model: Some("m-r29"),
                    effective_model: None,
                    profile_id: None,
                },
            )
            .unwrap()
            .unwrap();
            let instance_agg = aggregate_instance(&conn, instance).unwrap();
            let supply = aggregate_supply(&conn, "pvp_r29", "m-r29").unwrap();

            if proven {
                // (a) the stock covers the proven turn: 100/10, not 200/20.
                assert_eq!(rollup.session_input_tokens, Some(100));
                assert_eq!(rollup.session_output_tokens, Some(10));
                assert_eq!(instance_agg.events, 1);
                assert_eq!(instance_agg.input_tokens, Some(100));
                assert_eq!(instance_agg.output_tokens, Some(10));
                assert_eq!(supply.events, 1);
                assert_eq!(supply.input_tokens, Some(100));
                assert_eq!(supply.output_tokens, Some(10));
            } else {
                // (b) ambiguous: no proven extra flow and no safe coverage.
                assert_eq!(
                    rollup.session_input_tokens, None,
                    "unprovable turn beside scoped stock makes the bucket Unknown"
                );
                assert_eq!(rollup.session_output_tokens, None);
                assert_eq!(rollup.cache_read_tokens, None);
                assert_eq!(rollup.cache_creation_tokens, None);
                // The stock alone is the proven event; the ambiguous turn is
                // not counted as one.
                assert_eq!(instance_agg.events, 1);
                assert_eq!(instance_agg.input_tokens, None);
                assert_eq!(instance_agg.output_tokens, None);
                assert_eq!(supply.events, 1);
                assert_eq!(supply.input_tokens, None);
                assert_eq!(supply.output_tokens, None);
            }
        }
    }

    /// c-ctxusage r29 item 2: NULL-scope session rows are cumulative growth
    /// points, so every fold keeps only the NEWEST one. Two monotone stocks
    /// (100/10 then 150/15) agree as 150/15 across rollup, instance and
    /// supply — never summed to 250/25.
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
    /// Create an r28-era usage_events table: all identity columns present AND
    /// the r16 partial unique index already in place.
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

    /// Count surviving scoped turn rows of one lineage.
    fn lineage_turn_rows(conn: &Connection, instance: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM usage_events
             WHERE instance_id = ?1 AND scope = 'turn'
               AND scope_id = 'turn-1'
               AND COALESCE(source_session_id, '') = 'sess-codex'",
            params![instance],
            |row| row.get(0),
        )
        .unwrap()
    }

    /// c-ctxusage r30 item 1: the r29 turn backfill runs while the r16
    /// partial unique index is still present on an r28 database, so a legacy
    /// turn proven as (T, S) beside a later qualified replay (T, S) made the
    /// migration fail on first startup. The index must be dropped before the
    /// backfill and the duplicate lineage collapsed to the fold winner.
    #[test]
    fn r28_indexed_database_backfills_turn_lineage_without_collision() {
        let conn = Connection::open_in_memory().unwrap();
        create_r28_usage_table_with_turn_index(&conn);
        create_journal_table(&conn);
        let instance = "ins_r30a";
        // Legacy turn from the pre-scopeId producer (identity columns NULL).
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, profile_id, model, scope, mode,
                 metric_revision, source_session_id, scope_id, total_tokens,
                 input_tokens, output_tokens, cache_read_tokens,
                 cache_write_tokens, accounting, observed_at,
                 observed_at_source)
             VALUES (?1, 1, 'pvp_r29', 'm-r29', 'turn', 'snapshot',
                     1, NULL, NULL, 110, 100, 10, 0, 0, 'estimated',
                     '2020-01-01T00:00:00.000Z', 'ingest')",
            params![instance],
        )
        .unwrap();
        // The cumulative stock the resume replayed.
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, profile_id, model, scope, mode,
                 metric_revision, source_session_id, scope_id, total_tokens,
                 input_tokens, output_tokens, cache_read_tokens,
                 cache_write_tokens, accounting, observed_at,
                 observed_at_source)
             VALUES (?1, 2, 'pvp_r29', 'm-r29', 'session', 'snapshot',
                     1, 'sess-codex', 'sess-codex', 110, 100, 10, 0, 0,
                     'estimated', '2020-01-01T00:00:00.000Z', 'ingest')",
            params![instance],
        )
        .unwrap();
        // The later qualified replay of the SAME turn (the r16 index admits
        // it while the legacy row is still NULL-identity).
        conn.execute(
            "INSERT INTO usage_events
                (instance_id, seq, profile_id, model, scope, mode,
                 metric_revision, source_session_id, scope_id, total_tokens,
                 input_tokens, output_tokens, cache_read_tokens,
                 cache_write_tokens, accounting, observed_at,
                 observed_at_source)
             VALUES (?1, 3, 'pvp_r29', 'm-r29', 'turn', 'snapshot',
                     1, 'sess-codex', 'turn-1', 110, 100, 10, 0, 0,
                     'estimated', '2020-01-02T00:00:00.000Z', 'native')",
            params![instance],
        )
        .unwrap();
        // Retained evidence proves the legacy turn as turn-1/sess-codex.
        insert_journal_raw(
            &conn,
            &journal_usage_with_envelope(
                1,
                instance,
                "turn",
                Some("turn-1"),
                "2020-01-01T00:00:00.000Z",
                "known",
            ),
        );
        insert_journal_raw(
            &conn,
            &journal_usage_with_envelope(
                2,
                instance,
                "session",
                Some("sess-codex"),
                "2020-01-01T00:00:00.000Z",
                "known",
            ),
        );

        // The migration must succeed — the unique index is dropped before the
        // identity UPDATE that would otherwise violate it.
        migrate(&conn).unwrap();
        assert_eq!(
            lineage_turn_rows(&conn, instance),
            1,
            "duplicate lineage collapsed"
        );
        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: instance,
                kind: "codex",
                spec_model: Some("m-r29"),
                effective_model: None,
                profile_id: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(rollup.session_input_tokens, Some(100), "100/10, not 200/20");
        assert_eq!(rollup.session_output_tokens, Some(10));
        // The fold winner (newest accepted frame, seq 3, native time) survives.
        let (seq, source): (i64, String) = conn
            .query_row(
                "SELECT seq, observed_at_source FROM usage_events
                 WHERE instance_id = ?1 AND scope='turn' AND scope_id='turn-1'",
                params![instance],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(seq, 3, "the fold winner (qualified replay) survives");
        assert_eq!(source, "native");
        // The index is back in place after the migration.
        let index_present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='index' AND name='usage_events_turn_dedupe'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index_present, 1);

        // (b) A SECOND migration changes nothing: idempotent, index rebuilt,
        // still one row and the same totals.
        migrate(&conn).unwrap();
        assert_eq!(lineage_turn_rows(&conn, instance), 1);
        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: instance,
                kind: "codex",
                spec_model: Some("m-r29"),
                effective_model: None,
                profile_id: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(rollup.session_input_tokens, Some(100));
        assert_eq!(rollup.session_output_tokens, Some(10));
    }

    /// c-ctxusage r30 item 1 (c): an origin/main database with TWO NULL-scope
    /// turn rows whose retained evidence restores them both to the same
    /// (T, S). The first identity UPDATE alone creates identical indexed
    /// keys, so the index must already be gone; the duplicates collapse and
    /// the turn is counted once.
    #[test]
    fn origin_main_two_null_turns_restoring_to_one_lineage_collapse() {
        let conn = Connection::open_in_memory().unwrap();
        create_main_usage_table(&conn);
        create_journal_table(&conn);
        let instance = "ins_r30c";
        let at = "2020-01-01T00:00:00.000Z";
        // Two replayed NULL-identity rows for the one logical turn.
        insert_main_usage_row(&conn, instance, 1, "turn", 100, 10, at);
        insert_main_usage_row(&conn, instance, 3, "turn", 100, 10, at);
        insert_main_usage_row(&conn, instance, 2, "session", 100, 10, at);
        insert_journal_raw(
            &conn,
            &journal_usage_with_envelope(1, instance, "turn", Some("turn-1"), at, "known"),
        );
        insert_journal_raw(
            &conn,
            &journal_usage_with_envelope(3, instance, "turn", Some("turn-1"), at, "known"),
        );
        insert_journal_raw(
            &conn,
            &journal_usage_with_envelope(2, instance, "session", Some("sess-codex"), at, "known"),
        );

        migrate(&conn).unwrap();
        assert_eq!(
            lineage_turn_rows(&conn, instance),
            1,
            "the two restored rows collapse to one"
        );
        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: instance,
                kind: "codex",
                spec_model: Some("m-r29"),
                effective_model: None,
                profile_id: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(rollup.session_input_tokens, Some(100), "counted once");
        assert_eq!(rollup.session_output_tokens, Some(10));
        assert_eq!(rollup.turns, 1);
        // Idempotent re-run.
        migrate(&conn).unwrap();
        assert_eq!(lineage_turn_rows(&conn, instance), 1);
        let rollup = rollup_instance(
            &conn,
            &RollupRequest {
                instance_id: instance,
                kind: "codex",
                spec_model: Some("m-r29"),
                effective_model: None,
                profile_id: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(rollup.session_input_tokens, Some(100));
        assert_eq!(rollup.session_output_tokens, Some(10));
    }

    /// c-ctxusage r33 item 1: one Codex native session whose model changes
    /// mid-session. The adapter emits a per-turn flow AND a cumulative stock
    /// at every turn end, with the stock projected under the then-current
    /// model. Coverage is per (instance, native session) GLOBALLY across
    /// models: model A's turn is already inside model B's winning cumulative
    /// stock and must not be resurrected by supply(A)'s model filter.
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

    /// c-ctxusage r34 item 1: the unprovable-lineage migration shape combined
    /// with a model change. A retained NULL-lineage turn under model A (100 /
    /// $1) sits beside a scoped model-B cumulative stock (250 / $3). Global
    /// coverage makes the A turn AMBIGUOUS (its session cannot be proven), so
    /// it must not enter numeric sums — but supply(A) must carry the explicit
    /// ambiguity flag with zero proven events so the provider endpoint emits
    /// an Unknown-budget row instead of dropping the model entirely.
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
