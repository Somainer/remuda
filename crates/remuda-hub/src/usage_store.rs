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
    /// (`"native"`) or was stamped at journal ingest (`"ingest"`). Only a
    /// native timestamp may authoritatively repair the stored time; ingest
    /// fallbacks never overwrite one (c-ctxusage r4 item 5).
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
pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
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
    // c-ctxusage r4 item 5: remember whether observed_at is native-sourced or
    // an ingest fallback, so an unchanged-counters replay can repair the
    // historical native time. Legacy rows are conservatively 'ingest'.
    crate::store::ensure_column(conn, "usage_events", "observed_at_source", "TEXT")?;
    conn.execute(
        "UPDATE usage_events SET observed_at_source = 'ingest'
         WHERE observed_at_source IS NULL OR observed_at_source = ''",
        [],
    )?;
    // c-usagefu (c): native per-model context window reported on the payload.
    crate::store::ensure_column(conn, "usage_events", "native_context_window", "INTEGER")?;
    // Now the columns exist on both a fresh CREATE and an upgraded DB.
    //
    // Dedupe indexes are scope-specific (c-ctxusage r4 item 3):
    // - Turn rows (Claude message id): one revision-replaced row per message.
    // - Session rows (cumulative Grok/Codex stock): append-only GROWTH POINTS,
    //   never collapsed — rate windows need their history. Duplicate/historical
    //   replay points are frozen in Rust by their cumulative counters, not by a
    //   one-row unique index. The old shared index collapsed both scopes, so it
    //   is dropped on every prior schema.
    // c-usagefu r9 item 3: duplicate Message rows are consolidated into ONE
    // survivor per durable key (instance_id, scope, scope_id) carrying the
    // best counters/cost/attribution any sibling brought; the rest are
    // deleted and the unique indexes created — ALL in one transaction, so a
    // crash between delete and index creation can never leave a partially
    // collapsed database. Idempotent.
    consolidate_message_duplicates(conn)?;
    Ok(())
}

/// Collapse pre-r9 duplicate Message rows (same instance_id, scope,
/// scope_id) into a single survivor each, then create the dedupe indexes —
/// in ONE transaction (c-usagefu r9 item 3).
///
/// The survivor is the FIRST write (lowest rowid, preserving ingest
/// ordering). Ruling C (c-usagefu r10 item 3):
/// - counters/cost (total/input/output/cache buckets, cost_usd) are
///   ACCOUNTING and come from the AUTHORITATIVE sibling — the highest
///   metric_revision row that reports each value, newest rowid breaking a
///   revision tie. A rev-1 row reporting output 10/cost 0.01 and a rev-2 row
///   reporting output 20/cost 0.02 leave the survivor at 20/0.02, never the
///   first non-NULL 10/0.01. A bucket no sibling reports stays NULL (a
///   NULL-to-known repair still arrives from whichever revision carries it);
/// - revision: the maximum;
/// - profile/model/turn id/native context window/observed time are
///   ATTRIBUTION and merge best-known (any non-NULL sibling; native time over
///   ingest time), independent of which revision owned the accounting.
///
/// A second run changes nothing: one row per key makes the UPDATE/DELETE
/// no-ops and the index creates are `IF NOT EXISTS`.
fn consolidate_message_duplicates(conn: &Connection) -> rusqlite::Result<()> {
    // migrate() holds only `&Connection`; the migration runs once at DB open
    // on a single connection (no concurrent writers), so an unchecked
    // transaction is sound and still gives atomic commit/rollback.
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
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
             metric_revision = (
                 SELECT MAX(m2.metric_revision) FROM usage_events m2
                 WHERE m2.instance_id = usage_events.instance_id
                   AND m2.scope = 'message' AND m2.scope_id = usage_events.scope_id),
             total_tokens = (
                 SELECT m2.total_tokens FROM usage_events m2
                 WHERE m2.instance_id = usage_events.instance_id
                   AND m2.scope = 'message' AND m2.scope_id = usage_events.scope_id
                   AND m2.total_tokens IS NOT NULL
                 ORDER BY m2.metric_revision DESC, m2.rowid DESC LIMIT 1),
             input_tokens = (
                 SELECT m2.input_tokens FROM usage_events m2
                 WHERE m2.instance_id = usage_events.instance_id
                   AND m2.scope = 'message' AND m2.scope_id = usage_events.scope_id
                   AND m2.input_tokens IS NOT NULL
                 ORDER BY m2.metric_revision DESC, m2.rowid DESC LIMIT 1),
             output_tokens = (
                 SELECT m2.output_tokens FROM usage_events m2
                 WHERE m2.instance_id = usage_events.instance_id
                   AND m2.scope = 'message' AND m2.scope_id = usage_events.scope_id
                   AND m2.output_tokens IS NOT NULL
                 ORDER BY m2.metric_revision DESC, m2.rowid DESC LIMIT 1),
             cache_read_tokens = (
                 SELECT m2.cache_read_tokens FROM usage_events m2
                 WHERE m2.instance_id = usage_events.instance_id
                   AND m2.scope = 'message' AND m2.scope_id = usage_events.scope_id
                   AND m2.cache_read_tokens IS NOT NULL
                 ORDER BY m2.metric_revision DESC, m2.rowid DESC LIMIT 1),
             cache_write_tokens = (
                 SELECT m2.cache_write_tokens FROM usage_events m2
                 WHERE m2.instance_id = usage_events.instance_id
                   AND m2.scope = 'message' AND m2.scope_id = usage_events.scope_id
                   AND m2.cache_write_tokens IS NOT NULL
                 ORDER BY m2.metric_revision DESC, m2.rowid DESC LIMIT 1),
             cost_usd = (
                 SELECT m2.cost_usd FROM usage_events m2
                 WHERE m2.instance_id = usage_events.instance_id
                   AND m2.scope = 'message' AND m2.scope_id = usage_events.scope_id
                   AND m2.cost_usd IS NOT NULL
                 ORDER BY m2.metric_revision DESC, m2.rowid DESC LIMIT 1),
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
         CREATE UNIQUE INDEX IF NOT EXISTS usage_events_turn_dedupe
             ON usage_events(instance_id, scope, scope_id)
             WHERE scope = 'turn' AND scope_id IS NOT NULL;
         -- c-usagefu r7 item 5: codex Message rows are keyed on the durable
         -- per-response id; re-promoting/replaying a rollout byte-0 re-emits
         -- the same response and must not double the Message totals.
         CREATE UNIQUE INDEX IF NOT EXISTS usage_events_message_dedupe
             ON usage_events(instance_id, scope, scope_id)
             WHERE scope = 'message' AND scope_id IS NOT NULL;",
    )?;
    tx.commit()
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
    match (row.scope.as_str(), row.scope_id.as_deref()) {
        ("turn", Some(_)) => upsert_turn_snapshot(conn, row),
        ("session", Some(_)) => insert_session_growth_point(conn, row),
        ("session", None) => insert_legacy_session_point(conn, row),
        (_, None) => insert_row(conn, row),
        _ => insert_row(conn, row),
    }
}

/// Query every cumulative session stock a new scoped point must not move
/// backwards against: the point's OWN session id plus any LEGACY NULL-scope
/// session rows (c-ctxusage r5 item 3B). A byte-0 restart replay cannot
/// re-append a smaller stock than the old-format cumulative total.
fn existing_session_floors(
    conn: &Connection,
    row: &UsageEventRow,
) -> rusqlite::Result<Vec<SessionStockPoint>> {
    let mut stmt = conn.prepare(
        "SELECT total_tokens, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                cost_usd
         FROM usage_events
         WHERE instance_id = ?1 AND scope = 'session'
           AND (scope_id = ?2 OR scope_id IS NULL)",
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
/// historical/duplicate replays (c-ctxusage r4 item 3 / r5 item 3B).
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
             metric_revision, total_tokens, input_tokens, output_tokens,
             cache_read_tokens, cache_write_tokens, cost_usd, accounting,
             observed_at, observed_at_source, native_context_window)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
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
    Ok(inserted > 0)
}

/// Insert or improve a durable Message row keyed on
/// (instance_id, scope='message', scope_id). c-usagefu r8 item 5: a byte-0
/// replay re-emits the same response; if the existing row lacks turn_id or a
/// native timestamp and the incoming row carries one, update those fields
/// (and counters/cost when the existing row had them NULL). Returns true when
/// a row was inserted OR updated.
fn upsert_message_row(conn: &Connection, row: &UsageEventRow) -> rusqlite::Result<bool> {
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO usage_events
            (instance_id, seq, profile_id, model, scope, scope_id, turn_id, mode,
             metric_revision, total_tokens, input_tokens, output_tokens,
             cache_read_tokens, cache_write_tokens, cost_usd, accounting,
             observed_at, observed_at_source, native_context_window)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
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
    // Duplicate durable key: improve the stored row where the incoming row has
    // a value the stored one lacks (turn_id, native timestamp/source, NULL
    // counters/cost). Never overwrite a present value with NULL.
    //
    // c-usagefu r10 item 8 (Ruling C): a NULL-to-known ACCOUNTING repair is
    // allowed independent of attribution. A row ingested with COMPLETE
    // attribution (turn id + native time already present) but an UNKNOWN cost
    // must still receive the known cost on replay; the old guard ran the UPDATE
    // only while attribution was incomplete, so the cost stayed NULL forever.
    let updated = conn.execute(
        "UPDATE usage_events SET
             turn_id = COALESCE(turn_id, ?3),
             total_tokens = COALESCE(total_tokens, ?4),
             input_tokens = COALESCE(input_tokens, ?5),
             output_tokens = COALESCE(output_tokens, ?6),
             cache_read_tokens = COALESCE(cache_read_tokens, ?7),
             cache_write_tokens = COALESCE(cache_write_tokens, ?8),
             cost_usd = COALESCE(cost_usd, ?9),
             observed_at = CASE
                 WHEN observed_at_source = 'native' THEN observed_at
                 WHEN ?11 = 'native' THEN ?10
                 ELSE observed_at END,
             observed_at_source = CASE
                 WHEN observed_at_source = 'native' THEN 'native'
                 ELSE ?11 END
         WHERE instance_id = ?1 AND scope = 'message' AND scope_id = ?2
           AND (turn_id IS NULL
                OR (observed_at_source != 'native' AND ?11 = 'native')
                OR (total_tokens IS NULL AND ?4 IS NOT NULL)
                OR (input_tokens IS NULL AND ?5 IS NOT NULL)
                OR (output_tokens IS NULL AND ?6 IS NOT NULL)
                OR (cache_read_tokens IS NULL AND ?7 IS NOT NULL)
                OR (cache_write_tokens IS NULL AND ?8 IS NOT NULL)
                OR (cost_usd IS NULL AND ?9 IS NOT NULL))",
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
}

/// The stored state a scoped upsert conflicts with.
struct StoredSnapshot {
    counters: SnapshotCounters,
    observed_at: String,
    observed_at_source: String,
}

/// Insert or revise a Turn-scoped snapshot (one revision-replaced row per
/// assistant message id).
///
/// A later record of the same message carries the message's FINAL counters
/// (c-ctxusage r3 item 1): accept when any reported counter grows without one
/// decreasing. Independently, a NATIVE timestamp repairs a stored ingest-
/// fallback `observed_at` even when counters are identical; an ingest fallback
/// never overwrites a native time (c-ctxusage r4 item 5).
fn upsert_turn_snapshot(conn: &Connection, row: &UsageEventRow) -> rusqlite::Result<bool> {
    let stored: Option<StoredSnapshot> = conn
        .query_row(
            "SELECT total_tokens, input_tokens, output_tokens, cache_read_tokens,
                    cache_write_tokens, observed_at, observed_at_source
             FROM usage_events
             WHERE instance_id = ?1 AND scope = 'turn' AND scope_id = ?2",
            params![row.instance_id, row.scope_id],
            |r| {
                Ok(StoredSnapshot {
                    counters: SnapshotCounters {
                        total: r.get(0)?,
                        input: r.get(1)?,
                        output: r.get(2)?,
                        cache_read: r.get(3)?,
                        cache_write: r.get(4)?,
                    },
                    observed_at: r.get(5)?,
                    observed_at_source: r.get(6)?,
                })
            },
        )
        .ok();

    let Some(stored) = stored else {
        return insert_row(conn, row);
    };

    let incoming = SnapshotCounters::of(row);
    let (grew, decreased) = incoming.growth_against(&stored.counters);
    let counters_change = grew && !decreased;
    let time_repair = row.observed_at_source == "native"
        && stored.observed_at_source != "native"
        && row.observed_at != stored.observed_at;

    if !counters_change && !time_repair {
        return Ok(false);
    }

    if counters_change {
        let observed_at =
            if row.observed_at_source == "native" || stored.observed_at_source != "native" {
                row.observed_at.clone()
            } else {
                stored.observed_at.clone()
            };
        let observed_at_source =
            if row.observed_at_source == "native" || stored.observed_at_source != "native" {
                row.observed_at_source.clone()
            } else {
                stored.observed_at_source.clone()
            };
        conn.execute(
            "UPDATE usage_events SET
                seq = ?1,
                profile_id = ?2,
                model = ?3,
                mode = ?4,
                metric_revision = ?5,
                total_tokens = ?6,
                input_tokens = ?7,
                output_tokens = ?8,
                cache_read_tokens = ?9,
                cache_write_tokens = ?10,
                cost_usd = ?11,
                accounting = ?12,
                observed_at = ?13,
                observed_at_source = ?14,
                native_context_window = ?15
             WHERE instance_id = ?16 AND scope = 'turn' AND scope_id = ?17",
            params![
                row.seq,
                row.profile_id,
                row.model,
                row.mode,
                row.metric_revision,
                row.total_tokens,
                row.input_tokens,
                row.output_tokens,
                row.cache_read_tokens,
                row.cache_write_tokens,
                row.cost_usd,
                row.accounting,
                observed_at,
                observed_at_source,
                row.native_context_window,
                row.instance_id,
                row.scope_id,
            ],
        )?;
    } else {
        // Time-only repair: identical counters, just the authoritative time.
        conn.execute(
            "UPDATE usage_events SET
                seq = ?1,
                observed_at = ?2,
                observed_at_source = 'native'
             WHERE instance_id = ?3 AND scope = 'turn' AND scope_id = ?4",
            params![row.seq, row.observed_at, row.instance_id, row.scope_id],
        )?;
    }
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

/// Decode a bare protocol `U64` scalar — a canonical decimal string (`"2"`),
/// tolerating a bare JSON number from non-Rust producers — as an `i64`.
/// Distinct from [`knowledge_u64`], which unwraps a `Knowledge<U64>` enum's
/// `{state,value}` object (c-ctxusage r3 item 2).
fn scalar_revision(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return text.parse().ok();
    }
    value.as_i64()
}

/// Parse a bare protocol `U64` scalar (decimal string, accepting a bare JSON
/// integer from loose producers).
fn scalar_u64(value: &Value) -> Option<i64> {
    if let Some(text) = value.as_str() {
        return text.parse().ok();
    }
    value.as_i64()
}

/// Project one journal record into a usage row when it is `kind:"usage"`.
///
/// `profile_id`/`model` come from the instance row (the payload itself has no
/// model field — the driver priced against the model before sending).
#[must_use]
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
    let input_tokens = payload.get("inputTokens").and_then(knowledge_u64);
    let output_tokens = payload.get("outputTokens").and_then(knowledge_u64);
    let cache_read_tokens = payload.get("cacheReadTokens").and_then(knowledge_u64);
    let cache_write_tokens = payload.get("cacheWriteTokens").and_then(knowledge_u64);
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

/// Aggregated usage for one `(profile, model)` or instance.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageAggregate {
    /// Rows folded.
    pub events: i64,
    /// Summed total tokens.
    pub total_tokens: i64,
    /// Summed input tokens.
    pub input_tokens: i64,
    /// Summed output tokens.
    pub output_tokens: i64,
    /// Summed cost (decimal, estimated).
    pub cost_usd: f64,
}

impl UsageAggregate {
    /// Budget status against an estimated cap: ok → warn (estimate reached) →
    /// stop (estimate × 1.15). Costs are labeled estimates.
    #[must_use]
    pub fn budget_status(&self, max_usd: Option<f64>) -> BudgetStatus {
        let Some(max) = max_usd.filter(|v| *v > 0.0) else {
            return BudgetStatus::Ok;
        };
        if self.cost_usd >= max * BUDGET_STOP_FACTOR {
            BudgetStatus::Stop
        } else if self.cost_usd >= max {
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
    /// Under the estimate.
    Ok,
    /// At the estimate (warn).
    Warn,
    /// At estimate × 1.15 (stop band).
    Stop,
}

/// c-usagefu r10 Ruling B (items 4/5): ONE shared eligible-FLOW relation by
/// CALL identity, used by the rollup, both aggregates and the TPM windows.
///
/// Each native turn is counted EITHER from its Turn row OR from its Message
/// rows, never both.
///
/// * A MODERN Message (`turn_id` present) is counted only when either there is
///   no Turn row for that turn, or those Messages PROVABLY cover the whole
///   turn (see [`turn_fully_covered_sql`]). When coverage is partial — e.g. a
///   pre-fix accepted usage record without a response id was never emitted as
///   a Message — the Turn is counted and that turn's Messages are dropped.
/// * A LEGACY Message has no `turn_id` (pre-r6 wrote one Message + one Turn per
///   call). It is counted only when NO Turn row covers its time window; where
///   a covering Turn exists the Turn is counted and the legacy Message dropped.
/// * A Turn row is counted only when its modern Messages do NOT provably cover
///   it completely. A legacy-covered turn therefore always counts (its
///   Messages are the link-less legacy kind).
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
                   AND datetime({outer}.observed_at)
                       BETWEEN datetime(_ef_lt.observed_at,
                                       '-{LEGACY_TURN_LOOKBACK_SECS} seconds')
                           AND datetime(_ef_lt.observed_at,
                                       '+{LEGACY_TURN_SLACK_SECS} seconds')
             )
         )
         OR ({outer}.scope = 'turn' AND NOT ({cover_linked_turn})))"
    )
}

/// True when the Messages linked to turn `(inst, tid)` PROVABLY cover the whole
/// turn (Ruling B item 5):
///
/// - at least one linked Message carries a persisted response-level identity
///   (`scope_id`, a real response id or the durable fallback id the adapter
///   mints for a record without one);
/// - for every TOKEN bucket the Turn row actually reports, the linked Messages'
///   sum EQUALS it. A bucket the Turn leaves NULL (a sparse historical Turn
///   never carried total/cache-write) cannot disprove coverage and is not
///   compared; the Messages may additionally report buckets the Turn omitted.
///   When a response record was not persisted the bucket sum falls short of the
///   Turn and the Turn is billed instead. Cost is deliberately NOT part of the
///   proof: it is a priced projection that may legitimately differ from a
///   historical Turn, and item 6 makes cost follow the SAME eligible-flow
///   selection (covered → Message cost; uncovered → Turn cost).
fn turn_fully_covered_sql(inst: &str, tid: &str) -> String {
    let buckets = [
        "total_tokens",
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
    ];
    let mut conds = vec![format!(
        "EXISTS (
            SELECT 1 FROM usage_events _fc_e
            WHERE _fc_e.instance_id = {inst}
              AND _fc_e.scope = 'message'
              AND _fc_e.turn_id = {tid}
              AND _fc_e.scope_id IS NOT NULL
        )"
    )];
    for bucket in buckets {
        // Each condition is a full OR and MUST be parenthesized: the list is
        // joined with AND, and a bare `A IS NULL OR B = C` would let AND bind
        // tighter and silently flatten the coverage proof across buckets.
        conds.push(format!(
            "((SELECT _fc_t.{bucket} FROM usage_events _fc_t
              WHERE _fc_t.instance_id = {inst}
                AND _fc_t.scope = 'turn' AND _fc_t.scope_id = {tid}) IS NULL
             OR COALESCE((
                SELECT SUM(_fc_s.{bucket}) FROM usage_events _fc_s
                WHERE _fc_s.instance_id = {inst}
                  AND _fc_s.scope = 'message' AND _fc_s.turn_id = {tid}
             ), 0)
             = (SELECT _fc_t2.{bucket} FROM usage_events _fc_t2
                WHERE _fc_t2.instance_id = {inst}
                  AND _fc_t2.scope = 'turn' AND _fc_t2.scope_id = {tid}))"
        ));
    }
    conds.join(" AND ")
}

/// A response usage record is written DURING the turn, up to the whole turn
/// duration before the end-of-turn Turn snapshot. Bound how far before a Turn
/// row a link-less legacy Message may sit and still describe that same call.
const LEGACY_TURN_LOOKBACK_SECS: i64 = 600;
/// Small clock-skew slack after the Turn snapshot.
const LEGACY_TURN_SLACK_SECS: i64 = 5;

/// True when instance `instance_expr` has at least one eligible flow row
/// (see [`eligible_flow_predicate`]). Legacy NULL-scope Turn rows count.
fn has_eligible_flow_sql(instance_expr: &str) -> String {
    format!(
        "EXISTS (
            SELECT 1 FROM usage_events _ef_e
            WHERE _ef_e.instance_id = {instance_expr}
              AND {}
        )",
        eligible_flow_predicate("_ef_e")
    )
}

/// Token-sum CASE arm for the shared flow relation (`column` is one token
/// bucket on the outer `usage_events` row): a `WHEN … THEN …` fragment to
/// embed inside an outer CASE.
fn flow_case_arm(column: &str) -> String {
    format!(
        "WHEN {} THEN {column}",
        eligible_flow_predicate("usage_events")
    )
}

/// Cost column for an aggregate query.
///
/// Stock vs flow, per session then per instance (c-usagefu r4 item 1; r5
/// items 3/7):
/// - a `session` row is a CUMULATIVE STOCK. The billable value is the NEWEST
///   accepted point per `(instance_id, scope_id)` and those per-session
///   stocks are SUMMED — a single global newest row let one instance answer
///   for ten, and interleaving sessions flipped the number;
/// - scoped and LEGACY NULL-scope rows never mix. When an instance has ANY
///   scoped session row, NULL-scope rows are the pre-`scopeId` world and are
///   excluded from the partition set entirely (same rule as the token stock,
///   c-ctxusage r5 item 3B), so a session straddling the upgrade is not
///   double-counted; the legacy chain is used only when no scoped row exists;
/// - newest points are ranked over all ELIGIBLE session rows, including NULL
///   costs. When any partition's newest point is NULL (the driver-priced
///   running cost went Unknown after an unpriced turn — sticky Unknown), the
///   instance is STOCK-LESS for billing and its per-call FLOW estimates are
///   summed instead, so the budget band keeps advancing;
/// - per-call (`turn`/`message`) rows therefore count for exactly the
///   instances not fully covered by priced stock.
///
/// r9 items 4/5 and r10 Ruling B (item 6): eligible FLOW is the shared
/// call-identity relation ([`eligible_flow_predicate`]). Session STOCK is used,
/// per instance/session, ONLY when no eligible flow exists at all (legacy
/// NULL-scope Turns included in that decision); cumulative stock is never added
/// to, or used as a smaller-flow fallback against, the per-call sums. COST
/// follows the SAME decision as tokens (item 6): a priced Session snapshot on
/// an instance that also has eligible flow is ignored and the flow cost is
/// billed.
///
/// `r5_` CTEs: `r5_scoped_instances` (instances with any scoped session
/// row), `r5_eligible_stock` (newest point per scoped partition, plus the
/// legacy NULL chain where no scoped row exists), `r5_flow_instances`
/// (instances with at least one eligible flow row), `r9_stock_only_instances`
/// (instances with session rows and NO eligible flow). Pricing decisions are
/// precomputed once per statement; the flow subquery joins the CTEs rather
/// than running a correlated NOT EXISTS per row.
fn cost_select_sql(stock_predicate: &str, flow_predicate: &str, outer_predicate: &str) -> String {
    format!(
        "WITH r5_scoped_instances AS (
            SELECT DISTINCT instance_id FROM usage_events
            WHERE {stock_predicate} AND scope = 'session' AND scope_id IS NOT NULL
         ),
         r9_stock_only_instances AS (
            -- r9 item 5: session stock counts ONLY for instances with no
            -- eligible flow (legacy NULL-scope Turns included).
            SELECT DISTINCT s.instance_id FROM usage_events s
            WHERE {stock_predicate} AND s.scope = 'session'
              AND NOT EXISTS (
                  SELECT 1 FROM usage_events f
                  WHERE f.instance_id = s.instance_id
                    AND {flow_exists_f}
              )
         ),
         r5_eligible_stock AS (
            -- The single LATEST cumulative point per (instance, scope_id):
            -- tokens AND cost come from these rows only (r9 item 5), never
            -- summed over the whole cumulative history (stock 10 then 30 is
            -- 30, not 40). Legacy NULL-scope rows participate only when the
            -- instance has no scoped rows at all.
            SELECT instance_id, scope_id, cost, rowid AS stock_rowid FROM (
                SELECT instance_id, scope_id, rowid,
                       CAST(cost_usd AS REAL) AS cost,
                       ROW_NUMBER() OVER (
                           PARTITION BY instance_id, scope_id ORDER BY seq DESC
                       ) AS rn
                FROM usage_events
                WHERE {stock_predicate} AND scope = 'session'
                  AND (
                      scope_id IS NOT NULL
                      OR instance_id NOT IN (
                          SELECT instance_id FROM r5_scoped_instances
                      )
                  )
            )
            WHERE rn = 1
         ),
         r5_flow_instances AS (
            -- r10 item 6 (Ruling B): COST follows the exact eligible-flow
            -- decision as tokens. An instance bills per-call FLOW cost whenever
            -- it has any eligible flow row — even if it ALSO carries a priced
            -- Session snapshot. Only a genuinely stock-only instance (no
            -- eligible flow at all, see r9_stock_only_instances) is excluded
            -- here and billed from its latest stock instead.
            SELECT DISTINCT f.instance_id FROM usage_events f
            WHERE {flow_exists_f}
         )
         SELECT COUNT(*),
                -- r9 items 4/5: ONE token representation per call — the
                -- shared eligible-flow relation; session stock contributes
                -- ONLY its latest per-scope rows for instances with no
                -- eligible flow. Cumulative Session history is never summed
                -- onto flow.
                COALESCE(SUM(CASE
                    WHEN usage_events.scope = 'session'
                         AND usage_events.rowid IN (
                             SELECT stock_rowid FROM r5_eligible_stock
                             WHERE instance_id IN (
                                 SELECT instance_id FROM r9_stock_only_instances))
                         THEN total_tokens
                    {flow_total}
                END), 0),
                COALESCE(SUM(CASE
                    WHEN usage_events.scope = 'session'
                         AND usage_events.rowid IN (
                             SELECT stock_rowid FROM r5_eligible_stock
                             WHERE instance_id IN (
                                 SELECT instance_id FROM r9_stock_only_instances))
                         THEN input_tokens
                    {flow_input}
                END), 0),
                COALESCE(SUM(CASE
                    WHEN usage_events.scope = 'session'
                         AND usage_events.rowid IN (
                             SELECT stock_rowid FROM r5_eligible_stock
                             WHERE instance_id IN (
                                 SELECT instance_id FROM r9_stock_only_instances))
                         THEN output_tokens
                    {flow_output}
                END), 0),
                (SELECT COALESCE(SUM(cost), 0.0) FROM r5_eligible_stock
                 WHERE instance_id IN (SELECT instance_id FROM r9_stock_only_instances))
                +
                (SELECT COALESCE(SUM(CAST(flow.cost_usd AS REAL)), 0.0)
                 FROM usage_events flow
                 JOIN r5_flow_instances
                   ON r5_flow_instances.instance_id = flow.instance_id
                 WHERE {flow_predicate}
                   AND flow.scope != 'session'
                   AND {flow_cost})
         FROM usage_events WHERE {outer_predicate}",
        flow_exists_f = eligible_flow_predicate("f"),
        flow_total = flow_case_arm("total_tokens"),
        flow_input = flow_case_arm("input_tokens"),
        flow_output = flow_case_arm("output_tokens"),
        flow_cost = eligible_flow_predicate("flow"),
    )
}

/// Aggregate all persisted events for one instance.
pub fn aggregate_instance(
    conn: &Connection,
    instance_id: &str,
) -> rusqlite::Result<UsageAggregate> {
    let sql = cost_select_sql(
        "instance_id = ?1",
        "flow.instance_id = ?2",
        "instance_id = ?3",
    );
    conn.query_row(
        &sql,
        params![instance_id, instance_id, instance_id],
        |row| {
            Ok(UsageAggregate {
                events: row.get(0)?,
                total_tokens: row.get(1)?,
                input_tokens: row.get(2)?,
                output_tokens: row.get(3)?,
                cost_usd: row.get(4)?,
            })
        },
    )
}

/// Aggregate persisted events for one `(profile, model)`.
pub fn aggregate_supply(
    conn: &Connection,
    profile_id: &str,
    model: &str,
) -> rusqlite::Result<UsageAggregate> {
    // Six bindings: stock CTE (profile, model), flow subquery (profile,
    // model), outer count/token totals (profile, model).
    let sql = cost_select_sql(
        "profile_id = ?1 AND model = ?2",
        "flow.profile_id = ?3 AND flow.model = ?4",
        "profile_id = ?5 AND model = ?6",
    );
    conn.query_row(
        &sql,
        params![profile_id, model, profile_id, model, profile_id, model],
        |row| {
            Ok(UsageAggregate {
                events: row.get(0)?,
                total_tokens: row.get(1)?,
                input_tokens: row.get(2)?,
                output_tokens: row.get(3)?,
                cost_usd: row.get(4)?,
            })
        },
    )
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
    conn.query_row(
        &format!(
            "SELECT SUM(input_tokens), SUM(output_tokens)
             FROM usage_events
             WHERE instance_id = ?1 AND observed_at >= ?2
               AND observed_at_source = 'native'
               AND {eligible}",
            eligible = eligible_flow_predicate("usage_events")
        ),
        params![instance_id, since],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
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

/// One ordered cumulative-session point: the two rate buckets, its native
/// end time and the session scope id (NULL for legacy rows).
type SessionPointRow = (Option<i64>, Option<i64>, String, Option<String>);

/// Ordered session points; scoped or legacy NULL-scope only.
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
        "SELECT input_tokens, output_tokens, observed_at, scope_id
         FROM usage_events
         WHERE instance_id = ?1 AND scope = 'session' AND {predicate}
         ORDER BY scope_id ASC, observed_at ASC, seq ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_map(params![instance_id], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    })?
    .collect()
}

/// Diff ordered points, restarting the chain at every scope boundary.
fn diff_rows(rows: Vec<SessionPointRow>) -> rusqlite::Result<Vec<StockIncrement>> {
    let mut increments = Vec::new();
    let mut previous: Option<(Option<String>, Option<i64>, Option<i64>)> = None;
    for (input, output, end_at, scope_id) in rows {
        let boundary = previous
            .as_ref()
            .map(|(prev_scope, _, _)| *prev_scope != scope_id)
            .unwrap_or(true);
        if !boundary && let Some((_, prev_input, prev_output)) = &previous {
            let bucket = |current: Option<i64>, previous: Option<i64>| -> Option<i64> {
                match (current, previous) {
                    (Some(current), Some(previous)) => Some(if current < previous {
                        current
                    } else {
                        current - previous
                    }),
                    (Some(current), None) => Some(current),
                    _ => None,
                }
            };
            increments.push(StockIncrement {
                input: bucket(input, *prev_input),
                output: bucket(output, *prev_output),
                end_at,
            });
        }
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
    let mut input: Option<i64> = None;
    let mut output: Option<i64> = None;
    for increment in in_window {
        if let Some(value) = increment.input {
            *input.get_or_insert(0) += value;
        }
        if let Some(value) = increment.output {
            *output.get_or_insert(0) += value;
        }
    }
    Ok((input, output))
}

/// One session's latest cumulative point: the four counters and its native
/// time, one row per scope id.
type LatestStockPoint = (Option<i64>, Option<i64>, Option<i64>, Option<i64>, String);

/// The summed current cumulative stock across session ids, plus its newest
/// native time.
#[derive(Debug, Default, Clone)]
struct SessionStock {
    input: Option<i64>,
    output: Option<i64>,
    cache_read: Option<i64>,
    cache_write: Option<i64>,
    observed_at: Option<String>,
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
    // Newest point per scoped session id, in native time / seq order.
    let mut stmt = conn.prepare(
        "WITH ranked AS (
            SELECT input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                   observed_at, scope_id,
                   ROW_NUMBER() OVER (
                       PARTITION BY scope_id
                       ORDER BY observed_at DESC, seq DESC
                   ) AS rn
            FROM usage_events
            WHERE instance_id = ?1 AND scope = 'session' AND scope_id IS NOT NULL
         )
         SELECT input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, observed_at
         FROM ranked WHERE rn = 1
         ORDER BY observed_at DESC",
    )?;
    let points: Vec<LatestStockPoint> = stmt
        .query_map(params![instance_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !points.is_empty() {
        let mut stock = SessionStock::default();
        for (input, output, cache_read, cache_write, observed_at) in &points {
            stock.input = add_optional(stock.input, *input);
            stock.output = add_optional(stock.output, *output);
            stock.cache_read = add_optional(stock.cache_read, *cache_read);
            stock.cache_write = add_optional(stock.cache_write, *cache_write);
            stock.observed_at = Some(observed_at.clone());
        }
        // observed_at is the newest of the per-scope latest points.
        stock.observed_at = points.into_iter().map(|(_, _, _, _, at)| at).max();
        return Ok((stock, true));
    }
    // Legacy fallback: the newest NULL-scope row.
    let stock = conn
        .query_row(
            "SELECT input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, observed_at
             FROM usage_events
             WHERE instance_id = ?1 AND scope = 'session' AND scope_id IS NULL
             ORDER BY observed_at DESC, seq DESC LIMIT 1",
            params![instance_id],
            |r| {
                Ok(SessionStock {
                    input: r.get(0)?,
                    output: r.get(1)?,
                    cache_read: r.get(2)?,
                    cache_write: r.get(3)?,
                    observed_at: r.get(4)?,
                })
            },
        )
        .unwrap_or_default();
    Ok((stock, false))
}

fn add_optional(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a + b),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
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
fn latest_call_basket(conn: &Connection, instance_id: &str) -> rusqlite::Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT input_tokens, cache_read_tokens, cache_write_tokens
         FROM usage_events
         WHERE instance_id = ?1 AND scope IN ('message', 'turn')
         ORDER BY CASE scope WHEN 'message' THEN 0 ELSE 1 END,
                  observed_at DESC, seq DESC LIMIT 1",
            params![instance_id],
            |row| {
                let input: Option<i64> = row.get(0)?;
                let cache_read: Option<i64> = row.get(1)?;
                let cache_write: Option<i64> = row.get(2)?;
                Ok([input, cache_read, cache_write]
                    .into_iter()
                    .flatten()
                    .reduce(i64::saturating_add))
            },
        )
        .ok()
        .flatten())
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
    let instance_id = req.instance_id;
    // Turn count over the ELIGIBLE flow rows: distinct turn id of every
    // Message, distinct scope_id of every unmatched modern Turn, plus one
    // per legacy NULL-scope_id Turn (r7 item 8). Legacy pre-scopeId Turns
    // counted by seq fallback so upgraded instances do not report zero.
    //
    // Token sums use the same relation: a native Turn-only call and a
    // Message-represented call both contribute exactly once.
    let flow_input = format!("SUM(CASE {} END)", flow_case_arm("input_tokens"));
    let flow_output = format!("SUM(CASE {} END)", flow_case_arm("output_tokens"));
    let flow_cache_read = format!("SUM(CASE {} END)", flow_case_arm("cache_read_tokens"));
    let flow_cache_write = format!("SUM(CASE {} END)", flow_case_arm("cache_write_tokens"));
    let totals = conn.query_row(
        &format!(
            "SELECT
            COUNT(DISTINCT CASE
                WHEN scope='message' THEN turn_id
                WHEN scope='turn' AND scope_id IS NOT NULL AND NOT {matched} THEN scope_id
                WHEN scope='workflow-member' THEN scope_id END)
            + COUNT(CASE WHEN scope='turn' AND scope_id IS NULL THEN 1 END),
            {flow_input},
            {flow_output},
            {flow_cache_read},
            {flow_cache_write},
            COUNT(CASE WHEN scope='session' THEN 1 END),
            COUNT(*),
            MAX(CASE WHEN scope IN ('turn','message') AND observed_at_source='native' THEN observed_at END)
         FROM usage_events WHERE instance_id = ?1",
            matched = "EXISTS (SELECT 1 FROM usage_events _ef_n
                              WHERE _ef_n.instance_id = usage_events.instance_id
                                AND _ef_n.scope = 'message'
                                AND _ef_n.turn_id = usage_events.scope_id)",
        ),
        params![instance_id],
        |row| {
            let turns: i64 = row.get(0)?;
            Ok(RollupTotals {
                turns,
                turn_input: row.get(1)?,
                turn_output: row.get(2)?,
                turn_cache_read: row.get(3)?,
                turn_cache_write: row.get(4)?,
                has_session: row.get::<_, i64>(5)? > 0,
                last_turn_at: row.get(7)?,
            })
        },
    )?;
    // A row exists if there is any flow (turn/message with a distinct turn
    // identity) OR any other row (legacy pre-scopeId turn / session).
    let total_rows: i64 = conn.query_row(
        "SELECT COUNT(*) FROM usage_events WHERE instance_id = ?1",
        params![instance_id],
        |row| row.get(0),
    )?;
    if totals.turns == 0 && !totals.has_session && total_rows == 0 {
        return Ok(None);
    }

    // r9 item 5: does the instance have ANY eligible flow row? The decision
    // includes legacy NULL-scope Turns.
    let has_eligible_flow: bool = conn.query_row(
        &format!("SELECT {}", has_eligible_flow_sql("?1")),
        params![instance_id],
        |row| row.get(0),
    )?;

    // The cumulative stock exists only for session-emitting harnesses.
    let (stock, _stock_scoped) = if totals.has_session {
        latest_session_stock(conn, instance_id)?
    } else {
        (SessionStock::default(), false)
    };

    // Totals: eligible flow whenever any exists; the latest stock ONLY when
    // the instance/session is flow-less. Never flow + stock, never a
    // smaller-flow stock fallback (r9 items 4/5).
    let use_flow = !totals.has_session || has_eligible_flow;
    let (session_input, session_output, cache_read, cache_creation) = if use_flow {
        (
            totals.turn_input,
            totals.turn_output,
            totals.turn_cache_read,
            totals.turn_cache_write,
        )
    } else {
        (
            stock.input,
            stock.output,
            stock.cache_read,
            stock.cache_write,
        )
    };

    // Context basket: the newest per-request/turn call; for a stock-only
    // harness (or when no call rows exist) the current stock counters.
    let context_used_tokens = latest_call_basket(conn, instance_id)?.or_else(|| {
        [stock.input, stock.cache_read, stock.cache_write]
            .into_iter()
            .flatten()
            .reduce(i64::saturating_add)
    });

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

    // Rate windows are summed from NATIVE-timestamped flow rows only (an
    // ingest-stamped replay must not read as a burst). The route therefore
    // depends on whether a native flow row EXISTS, not on has_turn: legacy
    // NULL-scope Turn rows (upgraded pre-delivery DBs, source backfilled to
    // 'ingest') make has_turn true, but the native-only flow query would
    // return NULL for them, blanking TPM on an instance whose cumulative
    // Session growth points can still supply increments (r8 item 5).
    let has_native_flow: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM usage_events
                       WHERE instance_id = ?1
                         AND scope IN ('turn', 'message')
                         AND observed_at_source = 'native')",
        params![instance_id],
        |row| row.get(0),
    )?;

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

    let last_turn_at = totals.last_turn_at.or(stock.observed_at);

    Ok(Some(InstanceUsageRollup {
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
        // c-usagefu r10 item 3 (Ruling C): BOTH duplicates already carry known
        // accounting at DIFFERENT revisions — rev 1 reports output 10 / cost
        // 0.01, a rev-2 replay reports output 20 / cost 0.02. Keeping the
        // first non-NULL froze the survivor at 10 / 0.01 even though revision
        // 2 is authoritative. Counters/cost must follow the HIGHEST revision
        // (with a NULL bucket repaired from a known lower revision), while
        // attribution (turn id, native time, profile, model, mode, context
        // window) merges best-known. A second migration must be a no-op.
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
                 native_context_window)
             VALUES
                ('ins_rev', 1, 'message', 'r1', NULL, 'snapshot', 1,
                 10, 7, 10, NULL, NULL, '0.01',
                 '2026-09-15T00:00:00.000Z', 'ingest', NULL, NULL, NULL),
                ('ins_rev', 2, 'message', 'r1', 't1', 'snapshot', 2,
                 20, NULL, 20, 50, 3, '0.02',
                 '2026-09-15T00:00:05.000Z', 'native', 'pvp_rev', 'gpt-5.4',
                 200000);",
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
                        turn_id: r.get(9)?,
                        profile: r.get(10)?,
                        model: r.get(11)?,
                        context_window: r.get(12)?,
                        observed_at: r.get(13)?,
                        source: r.get(14)?,
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
            first.input,
            Some(7),
            "rev-2 leaves input NULL: the NULL is repaired from known rev-1 7"
        );
        assert_eq!(first.output, Some(20), "rev-2 output 20 wins over rev-1 10");
        assert_eq!(first.cache_read, Some(50));
        assert_eq!(first.cache_write, Some(3));
        assert_eq!(
            first.cost.as_deref(),
            Some("0.02"),
            "cost is accounting: rev-2 0.02 wins, never the first non-NULL 0.01"
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
            agg.output_tokens, 150,
            "covering Turn 100 (legacy Message dropped) + Turn-only 50"
        );
        assert!((agg.cost_usd - 0.15).abs() < 1e-9, "0.10 + 0.05, not 0.25");
        let supply = aggregate_supply(&conn, "pvp_legacy", "gpt-5.4").unwrap();
        assert_eq!(supply.output_tokens, 150);
        assert!((supply.cost_usd - 0.15).abs() < 1e-9);
        let rollup = rollup_fold(&conn, "ins_legacy", "codex", Some("gpt-5.4"))
            .unwrap()
            .unwrap();
        assert_eq!(rollup.session_output_tokens, Some(150));
    }

    #[test]
    fn partially_covered_turn_bills_the_turn_and_a_fully_covered_one_bills_messages() {
        // c-usagefu r10 item 5 (Ruling B): a Turn is suppressed in favour of
        // its Messages only when they PROVABLY cover the whole turn.
        //   t1: Turn 120 / $0.12 with ONE linked Message r1 = 100 / $0.10 —
        //       the other accepted record (20, no response_id) was never
        //       emitted as a Message, so coverage is incomplete -> count the
        //       Turn 120 and drop r1 (never 100, never 220).
        //   t2: Turn 120 / $0.12 fully covered by r2a = 100 plus r2b = 20
        //       persisted under the adapter's DURABLE FALLBACK response id —
        //       sums equal the Turn -> bill the two Messages, drop the Turn.
        // Both turns contribute 120 -> 240 output, $0.24.
        let conn = open_old_dual_schema();
        conn.execute_batch(
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
                ('ins_partial', 3, 'message', 'r2a', 't2', 'snapshot',
                 100, '0.10', '2030-01-01T00:00:04.000Z', 'native',
                 'pvp_partial', 'gpt-5.4'),
                ('ins_partial', 4, 'message',
                 '_remuda_codex_response:t2:7', 't2', 'snapshot',
                 20, '0.02', '2030-01-01T00:00:04.500Z', 'native',
                 'pvp_partial', 'gpt-5.4'),
                ('ins_partial', 5, 'turn', 't2', 't2', 'snapshot',
                 120, '0.12', '2030-01-01T00:00:05.000Z', 'native',
                 'pvp_partial', 'gpt-5.4');",
        )
        .unwrap();
        migrate(&conn).unwrap();

        let agg = aggregate_instance(&conn, "ins_partial").unwrap();
        assert_eq!(
            agg.output_tokens, 240,
            "partial t1 counts its Turn 120; fully covered t2 counts 100+20"
        );
        assert!((agg.cost_usd - 0.24).abs() < 1e-9, "0.12 + 0.12");
        let supply = aggregate_supply(&conn, "pvp_partial", "gpt-5.4").unwrap();
        assert_eq!(supply.output_tokens, 240);
        assert!((supply.cost_usd - 0.24).abs() < 1e-9);
        let rollup = rollup_fold(&conn, "ins_partial", "codex", Some("gpt-5.4"))
            .unwrap()
            .unwrap();
        assert_eq!(rollup.session_output_tokens, Some(240));
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
        // (the pre-r6 codex shape). After migration every row's source is
        // backfilled to 'ingest'; TPM must still come from the session-stock
        // increments, not be blanked by a native-only turn query that has_turn
        // routes to. Points: 100/60 twenty seconds ago, 160/84 two seconds ago
        // (increment 60/24 inside both windows); a legacy turn row at 30s.
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
        assert_eq!(total.total_tokens, 300, "the original two rows, not four");
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
        assert_eq!(total.total_tokens, 1500);
        assert!((total.cost_usd - 0.03).abs() < 1e-9);

        // Unknown-cost events are not projected as cost rows...
        let third = usage_record(3, 10, None);
        let third = project_usage_event(&third, Some("pvp_relay"), Some("gw/model-y[1m]")).unwrap();
        insert_usage_event(&conn, &third).unwrap();
        let supply = aggregate_supply(&conn, "pvp_relay", "gw/model-y[1m]").unwrap();
        assert_eq!(supply.events, 1);
        assert_eq!(supply.cost_usd, 0.0);

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
            (after_one.cost_usd - 0.01).abs() < 1e-9,
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
            (after_two.cost_usd - 0.03).abs() < 1e-9,
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
        assert!((agg.cost_usd - 0.03).abs() < 1e-9);
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
        assert!((agg.cost_usd - 0.03).abs() < 1e-9);
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
            (instance.cost_usd - 0.30).abs() < 1e-9,
            "real codex Message-stream sequence: 0.10 + 0.20 = 0.30, got {}",
            instance.cost_usd
        );
        let supply = aggregate_supply(&conn, "pvp_codex", "gpt-5.4").unwrap();
        assert!(
            (supply.cost_usd - 0.30).abs() < 1e-9,
            "supply matches the flow bill, got {}",
            supply.cost_usd
        );
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
            (agg.cost_usd - 0.10).abs() < 1e-9,
            "repaired cost is billable"
        );
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
        assert_eq!(agg.output_tokens, 300, "100 + 200, not doubled");
        assert!(
            (agg.cost_usd - 0.20).abs() < 1e-9,
            "two responses at 0.10, replay not billed: {}",
            agg.cost_usd
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
        assert_eq!(agg.input_tokens, 100, "40+60 Message, not +140 Session");
        assert_eq!(agg.output_tokens, 30, "10+20 Message, not 10+10+20+30");
        assert!((agg.cost_usd - 0.30).abs() < 1e-9, "cumulative cost 0.30");
        let supply = aggregate_supply(&conn, "pvp_mix", "gpt-5.4").unwrap();
        assert_eq!(supply.input_tokens, 100);
        assert_eq!(supply.output_tokens, 30);
        assert!((supply.cost_usd - 0.30).abs() < 1e-9);

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
            100
        );
        assert_eq!(
            aggregate_supply(&conn, "pvp_up", "gpt-5.4")
                .unwrap()
                .output_tokens,
            100
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
            (instance.cost_usd - 0.30).abs() < 1e-9,
            "dual-scope rows bill once (Message stream): 0.30, got {}",
            instance.cost_usd
        );
        // Output tokens: 3 Message rows x 10000 = 30000, NOT 60000.
        assert_eq!(
            instance.output_tokens, 30_000,
            "tokens are not doubled across Message+Turn"
        );
        let supply = aggregate_supply(&conn, "pvp_dual", "gpt-5.4").unwrap();
        assert!(
            (supply.cost_usd - 0.30).abs() < 1e-9,
            "supply also unduplicated, got {}",
            supply.cost_usd
        );
        assert_eq!(supply.output_tokens, 30_000);
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
            (aggregate_instance(&conn, "ins_sticky").unwrap().cost_usd - 0.10).abs() < 1e-9,
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
            (agg.cost_usd - 0.30).abs() < 1e-9,
            "sticky-NULL newest stock -> flow 0.10 + 0.20, got {}",
            agg.cost_usd
        );
        // The supply aggregate follows the same instance-level rule.
        let supply = aggregate_supply(&conn, "pvp_sticky", "gw/sticky").unwrap();
        assert!(
            (supply.cost_usd - 0.30).abs() < 1e-9,
            "supply advances on flow after the NULL newest point, got {}",
            supply.cost_usd
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
            (agg.cost_usd - 0.50).abs() < 1e-9,
            "scoped stock alone counts (no 0.40 + 0.50), got {}",
            agg.cost_usd
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
            (agg.cost_usd - 0.30).abs() < 1e-9,
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
        payload.insert("accounting".into(), json!("estimated"));
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
            (agg.cost_usd - 0.55).abs() < 1e-9,
            "newest per-session stocks summed (0.50 + 0.05), got {}",
            agg.cost_usd
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
            (supply.cost_usd - 10.0).abs() < 1e-9,
            "stocks summed across instances, got {}",
            supply.cost_usd
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
            (supply.cost_usd - 0.13).abs() < 1e-9,
            "A's flow 0.10 + B's flow 0.03 (A stock ignored), got {}",
            supply.cost_usd
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
            agg.output_tokens, 150,
            "flow tokens 100 + 50, stock ignored"
        );
        assert!(
            (agg.cost_usd - 0.15).abs() < 1e-9,
            "flow cost 0.10 + 0.05, never the 0.99 priced stock, got {}",
            agg.cost_usd
        );
        let supply = aggregate_supply(&conn, "pvp_cost6", "gpt-5.4").unwrap();
        assert_eq!(supply.output_tokens, 150);
        assert!((supply.cost_usd - 0.15).abs() < 1e-9);
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
            cost_usd: 4.9,
            ..Default::default()
        };
        assert_eq!(agg.budget_status(Some(5.0)), BudgetStatus::Ok);
        let agg = UsageAggregate {
            cost_usd: 5.0,
            ..Default::default()
        };
        assert_eq!(agg.budget_status(Some(5.0)), BudgetStatus::Warn);
        let agg = UsageAggregate {
            cost_usd: 5.76,
            ..Default::default()
        };
        assert_eq!(agg.budget_status(Some(5.0)), BudgetStatus::Stop);
        assert_eq!(agg.budget_status(None), BudgetStatus::Ok);
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
            scope_id: Some(format!("turn-{seq}")),
            turn_id: Some(format!("turn-{seq}")),
            mode: "snapshot".into(),
            metric_revision: 1,
            total_tokens: None,
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
            cost_usd: None,
            accounting: "estimated".into(),
            observed_at: observed_at.into(),
            observed_at_source: "native".into(),
            native_context_window: None,
        }
    }

    /// Three real `message.usage` frames recorded from a Claude Code 2.1.x
    /// transcript on the dev host (assistant records, per turn):
    ///   `{input_tokens, cache_creation_input_tokens, cache_read_input_tokens, output_tokens}`
    /// = (4794, 0, 29496, 260), (1839, 0, 33592, 185), (1223, 0, 34616, 144).
    /// Timestamps are re-anchored to exercise the TPM windows; the counters
    /// are byte-for-byte the native record.
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
            mode: "snapshot".into(),
            metric_revision: rev,
            total_tokens: Some(input + output),
            input_tokens: Some(input),
            output_tokens: Some(output),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            cost_usd: None,
            accounting: "estimated".into(),
            observed_at: format!("2026-10-06T00:00:0{seq}.000Z"),
            observed_at_source: "native".into(),
            native_context_window: None,
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
        let rollup = rollup_fold(&conn, "ins_g", "grok", Some("grok-x"))
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
            mode: "snapshot".into(),
            metric_revision: seq,
            total_tokens: Some(input + output),
            input_tokens: Some(input),
            output_tokens: Some(output),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            cost_usd: None,
            accounting: "estimated".into(),
            observed_at: format!("2026-10-06T00:00:0{seq}.000Z"),
            observed_at_source: "native".into(),
            native_context_window: None,
        };
        let cumulative = |seq: i64, input: i64, output: i64| UsageEventRow {
            scope: "session".into(),
            scope_id: Some("codex-session".into()),
            turn_id: None,
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
        assert!(insert(
            &conn,
            &scoped_record(
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
            )
        ));
        assert!(insert(
            &conn,
            &scoped_record(
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

        // Exactly one durable row per plain message id, with FINAL counters.
        let input_of = |id: &str| -> Option<i64> {
            conn.query_row(
                "SELECT input_tokens FROM usage_events WHERE scope_id=?1 AND scope='turn'",
                params![id],
                |row| row.get(0),
            )
            .ok()
            .flatten()
        };
        assert_eq!(
            input_of("msg-rev"),
            Some(250),
            "revised final counters replace legacy/provisional"
        );
        assert_eq!(input_of("msg-int"), Some(333), "interrupted turn finalised");
        assert_eq!(input_of("msg-a"), Some(111), "superseded turn finalised");
        assert_eq!(
            input_of("msg-b"),
            Some(222),
            "end-of-stream finalises the last turn"
        );
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM usage_events WHERE scope='turn'",
                [],
                |row| row.get(0)
            )
            .unwrap(),
            4,
            "one turn row per message id — requestId namespacing/legacy not duplicated"
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

    /// r4 item 5: a replay with IDENTICAL counters but the correct historical
    /// native time repairs an ingest-fallback observed_at — lastTurnAt becomes
    /// historical and the rate windows empty — with no counter change.
    #[test]
    fn unchanged_counters_replay_repairs_native_time_and_empties_windows() {
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

        // Replay: identical counters, now with the real historical native time.
        let historical = scoped_record(
            2,
            "ins_r5",
            "turn",
            Some("msg-x"),
            1,
            250,
            0,
            0,
            0,
            Some("2020-01-01T00:00:00.000Z"),
        );
        assert!(
            insert(&conn, &historical),
            "a native time repair is accepted despite identical counters"
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
            mode: "snapshot".into(),
            metric_revision: 1,
            total_tokens: Some(input + output),
            input_tokens: Some(input),
            output_tokens: Some(output),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            cost_usd: None,
            accounting: "estimated".into(),
            observed_at: at.into(),
            observed_at_source: "native".into(),
            native_context_window: None,
        }
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

    /// r5 item 3B: legacy NULL-scope cumulative stock floors a restarted
    /// scoped replay; a smaller historical stock is frozen, growth accepted.
    #[test]
    fn legacy_null_scope_stock_floors_a_restarted_scoped_replay() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        assert!(
            insert_usage_event(
                &conn,
                &legacy_session_row(1, "ins_3b", 1000, 100, "2020-01-01T00:00:00.000Z")
            )
            .unwrap()
        );
        let replay_smaller =
            scoped_record(10, "ins_3b", "session", Some("s1"), 1, 100, 10, 0, 0, None);
        assert!(
            !insert(&conn, &replay_smaller),
            "a scoped stock below the legacy cumulative floor is frozen"
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
            "scoped world ignores the legacy row; the small replay never clobbered 1000"
        );
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
}
