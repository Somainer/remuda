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
    /// Native scope id (the assistant message id for a turn snapshot). Drives
    /// durable re-hydration dedupe: the same transcript re-mapped after a
    /// rebind re-emits the same `(scope, scope_id)`, and INSERT OR IGNORE on
    /// the partial unique index drops the duplicate projection.
    pub scope_id: Option<String>,
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
    // c-ctxusage r10 item 2: the obsolete shared unique index
    // (`instance_id, scope, scope_id` WHERE scope_id IS NOT NULL) MUST be
    // dropped BEFORE the retained-journal backfill stamps NULL-scope rows —
    // two legacy rows proving one session would otherwise violate it mid-
    // migration and the Hub database could never open (every retry failed at
    // this statement). Drop → backfill → collapse identical rows that the
    // stamp makes duplicates → create the scope-specific index, all in ONE
    // transaction so an interruption never leaves the schema half-migrated.
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch("DROP INDEX IF EXISTS usage_events_scope_dedupe;")?;
    // c-ctxusage r9 item 5: rows written in the pre-scopeId world have a NULL
    // scope_id, but the matching retained journal payload (keyed by
    // (instance_id, seq)) can still PROVE which session the cumulative stock
    // belonged to. Provenance comes ONLY from our own journal — never guessed.
    backfill_legacy_scope_ids(&tx)?;
    consolidate_identical_scoped_rows(&tx)?;
    // c-ctxusage r11 item 1: rows imported by the backfill (and any scoped
    // chains last written by pre-acceptance-rule code) were never run through
    // the cumulative-growth rule, so a stale historical replay can sit at a
    // HIGHER rowid than the valid high-water stock. Replay the same rule live
    // inserts use, in acceptance (rowid) order, before rowid order is allowed
    // to mean "accepted history" for current-stock selection and floors.
    normalise_scoped_session_chains(&tx)?;
    // Dedupe index is scope-specific (c-ctxusage r4 item 3):
    // - Turn rows (Claude message id): one revision-replaced row per message.
    // - Session rows (cumulative Grok/Codex stock): append-only GROWTH POINTS,
    //   never collapsed — rate windows need their history. Duplicate/historical
    //   replay points are frozen in Rust by their cumulative counters, not by a
    //   one-row unique index.
    tx.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS usage_events_turn_dedupe
             ON usage_events(instance_id, scope, scope_id)
             WHERE scope = 'turn' AND scope_id IS NOT NULL;",
    )?;
    tx.commit()?;
    Ok(())
}

/// Collapse rows that became exact duplicates after the retained-journal
/// scope-id backfill: same `(instance_id, scope, scope_id)` AND identical
/// counters in every bucket are the same cumulative point replayed at another
/// journal seq, so keeping both would double-count a stock and violate a
/// unique index. The oldest durable identity (smallest rowid) wins; its
/// `observed_at` is retained. Runs inside [`migrate`]'s transaction
/// (c-ctxusage r10 item 2).
fn consolidate_identical_scoped_rows(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM usage_events
         WHERE scope_id IS NOT NULL
           AND rowid NOT IN (
               SELECT MIN(rowid)
               FROM usage_events
               WHERE scope_id IS NOT NULL
               GROUP BY instance_id, scope, scope_id,
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
                output_tokens, cache_read_tokens, cache_write_tokens
         FROM usage_events
         WHERE scope = 'session' AND scope_id IS NOT NULL
         ORDER BY instance_id ASC, scope_id ASC, rowid ASC",
    )?;
    let rows = stmt.query_map([], |r| {
        let rowid: i64 = r.get(0)?;
        let instance_id: String = r.get(1)?;
        let scope_id: String = r.get(2)?;
        let counters = SnapshotCounters {
            total: r.get(3)?,
            input: r.get(4)?,
            output: r.get(5)?,
            cache_read: r.get(6)?,
            cache_write: r.get(7)?,
        };
        Ok((rowid, instance_id, scope_id, counters))
    })?;
    let mut current_key: Option<(String, String)> = None;
    let mut accepted: Vec<SnapshotCounters> = Vec::new();
    let mut stale_rowids: Vec<i64> = Vec::new();
    for entry in rows {
        let (rowid, instance_id, scope_id, counters) = entry?;
        let key = Some((instance_id, scope_id));
        if key != current_key {
            current_key = key;
            accepted.clear();
        }
        if freezes_against(&counters, &accepted) {
            stale_rowids.push(rowid);
        } else {
            accepted.push(counters);
        }
    }
    drop(stmt);
    let mut removed = 0;
    for rowid in stale_rowids {
        removed += conn.execute("DELETE FROM usage_events WHERE rowid = ?1", params![rowid])?;
    }
    Ok(removed)
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
) -> rusqlite::Result<Vec<SnapshotCounters>> {
    let mut stmt = conn.prepare(
        "SELECT total_tokens, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens
         FROM usage_events
         WHERE instance_id = ?1 AND scope = 'session'
           AND scope_id = ?2",
    )?;
    let floors = stmt
        .query_map(params![row.instance_id, row.scope_id], |r| {
            Ok(SnapshotCounters {
                total: r.get(0)?,
                input: r.get(1)?,
                output: r.get(2)?,
                cache_read: r.get(3)?,
                cache_write: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(floors)
}

/// Content-ordered acceptance against a set of existing stocks.
fn freezes_against(incoming: &SnapshotCounters, existing: &[SnapshotCounters]) -> bool {
    for stored in existing {
        if incoming == stored {
            return true;
        }
        let (_grew, decreased) = incoming.growth_against(stored);
        if decreased {
            return true;
        }
    }
    false
}

/// Append a new scoped cumulative-session growth point, freezing
/// historical/duplicate replays (c-ctxusage r4 item 3, refined in r8 item 8).
///
/// The cumulative stock is monotonic by contract WITHIN a session, so
/// acceptance is content-based against the point's own `scope_id` chain only:
/// - identical counters already recorded for THIS session -> frozen;
/// - any reported bucket smaller than this session's current stock -> frozen;
/// - otherwise the point is appended.
///
/// Legacy NULL-scope rows carry no session identity and therefore never floor
/// a scoped point (r8 item 8).
fn insert_session_growth_point(conn: &Connection, row: &UsageEventRow) -> rusqlite::Result<bool> {
    let incoming = SnapshotCounters::of(row);
    let floors = existing_session_floors(conn, row)?;
    if freezes_against(&incoming, &floors) {
        return Ok(false);
    }
    insert_row(conn, row)
}

/// Legacy (pre-`scopeId`) session row: content-ordered against other legacy
/// rows only. Scoped points are the newer world and are summed separately in
/// the rollup; legacy totals are read at all only while no scoped rows exist.
fn insert_legacy_session_point(conn: &Connection, row: &UsageEventRow) -> rusqlite::Result<bool> {
    let incoming = SnapshotCounters::of(row);
    let mut stmt = conn.prepare(
        "SELECT total_tokens, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens
         FROM usage_events
         WHERE instance_id = ?1 AND scope = 'session' AND scope_id IS NULL",
    )?;
    let existing: Vec<SnapshotCounters> = stmt
        .query_map(params![row.instance_id], |r| {
            Ok(SnapshotCounters {
                total: r.get(0)?,
                input: r.get(1)?,
                output: r.get(2)?,
                cache_read: r.get(3)?,
                cache_write: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if freezes_against(&incoming, &existing) {
        return Ok(false);
    }
    insert_row(conn, row)
}

/// Generic append (`INSERT OR IGNORE` on the `(instance_id, seq)` PK).
fn insert_row(conn: &Connection, row: &UsageEventRow) -> rusqlite::Result<bool> {
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO usage_events
            (instance_id, seq, profile_id, model, scope, scope_id, mode,
             metric_revision, total_tokens, input_tokens, output_tokens,
             cache_read_tokens, cache_write_tokens, cost_usd, accounting,
             observed_at, observed_at_source)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        params![
            row.instance_id,
            row.seq,
            row.profile_id,
            row.model,
            row.scope,
            row.scope_id,
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
        ],
    )?;
    Ok(inserted > 0)
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
    /// (`input + cache_read + cache_write + output`), so it advances with it.
    fn merged_over(self, retained: &SnapshotCounters) -> SnapshotCounters {
        let pick = |incoming: Option<i64>, kept: Option<i64>| incoming.or(kept);
        let input = pick(self.input, retained.input);
        let output = pick(self.output, retained.output);
        let cache_read = pick(self.cache_read, retained.cache_read);
        let cache_write = pick(self.cache_write, retained.cache_write);
        // Derive only when the incoming frame did not itself report a total:
        // the retained total is stale relative to a merged basket that moved.
        let total = self.total.or_else(|| {
            [input, cache_read, cache_write, output]
                .into_iter()
                .flatten()
                .reduce(i64::saturating_add)
        });
        SnapshotCounters {
            total,
            input,
            output,
            cache_read,
            cache_write,
        }
    }
}

/// The stored state a scoped upsert conflicts with.
struct StoredSnapshot {
    counters: SnapshotCounters,
    revision: i64,
    cost_usd: Option<String>,
    observed_at: String,
    observed_at_source: String,
}

/// Insert or revise a Turn-scoped snapshot (one revision-replaced row per
/// assistant message id).
///
/// Revision authority (c-ctxusage r13 item 3): a NEWER snapshot for the same
/// `(instance, scope, scope_id)` is the source's correction and replaces every
/// bucket it explicitly reports even when the value moves DOWN (output 20 →
/// 15), keeping buckets it does not report; an OLDER snapshot never overrides
/// anything; an EQUAL-revision snapshot may only fill an unreported bucket or
/// advance a reported one (the replay-safe monotone rule). Derived totals are
/// recomputed from the merged basket. Cumulative SESSION snapshots have their
/// own monotone acceptance path and are intentionally not governed here.
///
/// Cost (c-ctxusage r13 item 4): a frame that does not REPORT a cost leaves
/// the stored one in place — an unreported cost means "not reported", never
/// zero, so a token-only partial update never erases a known cost. An
/// explicitly reported cost on an equal-or-newer revision is authoritative on
/// its own, so a cost-only frame updates the row without any token growth; an
/// older-revision cost is frozen with the rest of the snapshot.
///
/// Independently, a NATIVE observation repairs a stored ingest-fallback
/// provenance even when counters are identical AND the timestamp string
/// equals the fallback (r11 item 5); an ingest fallback never overwrites a
/// native time (c-ctxusage r4 item 5).
fn upsert_turn_snapshot(conn: &Connection, row: &UsageEventRow) -> rusqlite::Result<bool> {
    let stored: Option<StoredSnapshot> = conn
        .query_row(
            "SELECT total_tokens, input_tokens, output_tokens, cache_read_tokens,
                    cache_write_tokens, metric_revision, cost_usd, observed_at,
                    observed_at_source
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
                    revision: r.get(5)?,
                    cost_usd: r.get(6)?,
                    observed_at: r.get(7)?,
                    observed_at_source: r.get(8)?,
                })
            },
        )
        .ok();

    let Some(stored) = stored else {
        return insert_row(conn, row);
    };

    let incoming = SnapshotCounters::of(row);
    let counters_change = if row.metric_revision > stored.revision {
        // Authoritative correction: any REPORTED basket that differs wins,
        // downwards included; unreported baskets are retained below. The
        // derived total is excluded — it is recomputed after the merge.
        incoming
            .token_baskets()
            .into_iter()
            .zip(stored.counters.token_baskets())
            .any(|(incoming, kept)| incoming.is_some_and(|value| Some(value) != kept))
    } else if row.metric_revision < stored.revision {
        // A stale revision never overrides a newer correction.
        false
    } else {
        // Equal revision (the common print/result case): accept a fill or an
        // advance, reject any reported decrease.
        let (grew, decreased) = incoming.growth_against(&stored.counters);
        grew && !decreased
    };
    // An explicitly reported cost is authoritative on an equal-or-newer
    // revision even when no token bucket moved (a cost-only update); a frame
    // with no reported cost contributes nothing here and keeps the stored one
    // via the merge below. An older revision's cost is frozen with its
    // counters.
    let cost_change = row.metric_revision >= stored.revision
        && row.cost_usd.is_some()
        && row.cost_usd != stored.cost_usd;
    // Independently, a NATIVE observation repairs a stored ingest-fallback
    // PROVENANCE even when the counters and timestamp string are unchanged:
    // the native source carrying the exact fallback string still proves the
    // time was native rather than assumed, so the inequality of the strings
    // is not required (c-ctxusage r11 item 5). An ingest fallback never
    // overwrites a native time.
    let time_repair = row.observed_at_source == "native" && stored.observed_at_source != "native";

    if !counters_change && !cost_change && !time_repair {
        return Ok(false);
    }

    if counters_change || cost_change {
        // r12 item 2: merge unreported incoming buckets with the retained
        // counters so a partial frame (e.g. a later print result carrying only
        // output_tokens) never erases a known counter; derive the total from
        // the merged basket when the incoming row did not report one. r13
        // item 4: the same merge for cost — an unreported incoming cost keeps
        // the known one instead of overwriting it with NULL.
        let merged = incoming.merged_over(&stored.counters);
        let merged_cost = row.cost_usd.clone().or_else(|| stored.cost_usd.clone());
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
                observed_at_source = ?14
             WHERE instance_id = ?15 AND scope = 'turn' AND scope_id = ?16",
            params![
                row.seq,
                row.profile_id,
                row.model,
                row.mode,
                row.metric_revision,
                merged.total,
                merged.input,
                merged.output,
                merged.cache_read,
                merged.cache_write,
                merged_cost,
                row.accounting,
                observed_at,
                observed_at_source,
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

/// Aggregate all persisted events for one instance.
pub fn aggregate_instance(
    conn: &Connection,
    instance_id: &str,
) -> rusqlite::Result<UsageAggregate> {
    conn.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(total_tokens), 0),
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(CAST(cost_usd AS REAL)), 0.0)
         FROM usage_events WHERE instance_id = ?1",
        params![instance_id],
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
    conn.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(total_tokens), 0),
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(CAST(cost_usd AS REAL)), 0.0)
         FROM usage_events WHERE profile_id = ?1 AND model = ?2",
        params![profile_id, model],
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

/// Kind-level fallback context windows, matching the web's own table before
/// the model catalog was available. Generic/terminal kinds report nothing.
fn kind_context_window(kind: &str) -> Option<i64> {
    match kind {
        "claude" | "codex" | "agy" => Some(200_000),
        "grok" => Some(128_000),
        _ => None,
    }
}

/// Resolve a session's context window: explicit `[1m]` long-context tag,
/// then the static model catalog, then the harness-kind fallback.
#[must_use]
pub fn context_window_tokens(kind: &str, model: Option<&str>) -> Option<i64> {
    if let Some(model) = model {
        let trimmed = model.trim();
        if trimmed.to_ascii_lowercase().ends_with("[1m]") {
            return Some(1_000_000);
        }
        if let Some(row) = crate::model_catalog::lookup(trimmed) {
            return Some(row.context_window as i64);
        }
    }
    kind_context_window(kind)
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

/// Sum the per-call (scope='turn') token flow within a rate window. Session
/// snapshots are a CUMULATIVE STOCK replaced in place, never a per-window flow,
/// so they are excluded here; the caller substitutes timestamped stock
/// increments only when the harness emits no turn rows at all (Grok) —
/// c-ctxusage r3 item 6 / r4 item 6.
fn sum_turn_tokens_since(
    conn: &Connection,
    instance_id: &str,
    since: &str,
) -> rusqlite::Result<(Option<i64>, Option<i64>)> {
    conn.query_row(
        "SELECT SUM(input_tokens), SUM(output_tokens)
         FROM usage_events
         WHERE instance_id = ?1 AND scope = 'turn' AND observed_at >= ?2
           AND observed_at_source = 'native'",
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

/// One ordered cumulative-session point: the two rate buckets, its observed
/// time, the session scope id (NULL for legacy rows) and the time source.
type SessionPointRow = (Option<i64>, Option<i64>, String, Option<String>, String);

/// Ordered session points; scoped or legacy NULL-scope only. EVERY accepted
/// point is walked — including ingest-stamped historical replays — because a
/// timestamp-free replay still ADVANCES the cumulative baseline: skipping it
/// made the next native point diff against a stale stock and inflate a single
/// increment (native 100 → untimed 120, 140 → native 150 read as +50 instead
/// of the true +10). Only the increment EMISSION is native-gated
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
        "SELECT input_tokens, output_tokens, observed_at, scope_id, observed_at_source
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
/// a NATIVE-timed point against its IMMEDIATE predecessor (whatever that
/// predecessor's source), and only when the predecessor exists. A native
/// point with no predecessor, or an interval whose predecessor counters are
/// unknown, yields no increment — the interval is unknown, never the stock
/// (c-ctxusage r9 item 3). An unknown PREDECESSOR bucket stays unknown rather
/// than being treated as zero: unknown→150 cannot be read as a 150 increment
/// (c-ctxusage r10 item 4).
fn diff_rows(rows: Vec<SessionPointRow>) -> rusqlite::Result<Vec<StockIncrement>> {
    let mut increments = Vec::new();
    let mut previous: Option<(Option<String>, Option<i64>, Option<i64>)> = None;
    for (input, output, end_at, scope_id, source) in rows {
        let boundary = previous
            .as_ref()
            .map(|(prev_scope, _, _)| *prev_scope != scope_id)
            .unwrap_or(true);
        if !boundary
            && source == "native"
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
    if !points.is_empty() {
        let mut stock = SessionStock::default();
        for (input, output, cache_read, cache_write, observed_at) in &points {
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
    // r10 item 3). Legacy rows predate source tracking and are conservatively
    // 'ingest', so native_observed_at stays None — a legacy cumulative total
    // cannot author a lastTurnAt.
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
        (Some(a), Some(b)) => Some(a + b),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// The newest per-call (scope='turn') basket: fresh input plus every cached
/// bucket of the most recent model call — what the NEXT request will carry. A
/// cumulative session snapshot is the wrong source here (it spans the whole
/// session), so turn rows take precedence even when the session row is the
/// newest by seq.
///
/// c-ctxusage r4 item 4: "newest" is the NATIVE time, not the ingest seq — a
/// late historical correction (replay of an older message at a high seq) must
/// not become the current context. Ties break deterministically on seq.
fn latest_turn_basket(conn: &Connection, instance_id: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT input_tokens, cache_read_tokens, cache_write_tokens
         FROM usage_events
         WHERE instance_id = ?1 AND scope = 'turn'
         ORDER BY observed_at DESC, seq DESC LIMIT 1",
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
}

/// Session-wide totals of one rollup query.
struct RollupTotals {
    turns: i64,
    /// Additive sums over scope='turn' rows; used for the totals only when no
    /// cumulative session snapshot exists (the Claude shape).
    turn_input: Option<i64>,
    turn_output: Option<i64>,
    turn_cache_read: Option<i64>,
    turn_cache_write: Option<i64>,
    has_turn: bool,
    has_session: bool,
    last_turn_at: Option<String>,
}

/// Fold every persisted usage event of one instance into
/// [`InstanceUsageRollup`]. Returns `None` when the session has no usage
/// observations yet, so the field stays off the instance record entirely.
///
/// ONE stock-vs-flow rule (c-ctxusage r3 item 6), applied identically to the
/// totals, the context basket and the rate windows:
/// - A **Session** row is a cumulative STOCK replaced in place by revision
///   (Grok emits only that; Codex emits it alongside per-turn rows).
/// - A **Turn** row is a per-call FLOW (Claude emits only turns; Codex emits
///   both).
///
/// Session totals come from the latest cumulative session snapshot whenever one
/// exists — correct even when Turn coverage is partial — otherwise from the sum
/// of turn flows. The context basket is the latest turn basket (never the
/// cumulative session total), falling back to the stock. The rate windows sum
/// turn flows; the stock is substituted only when no turn flows exist. This
/// keeps the session snapshot from vanishing when a turn row is present and
/// from being summed on top of the turns (the Codex inflated context / double
/// TPM count).
pub fn rollup_instance(
    conn: &Connection,
    instance_id: &str,
    kind: &str,
    model: Option<&str>,
) -> rusqlite::Result<Option<InstanceUsageRollup>> {
    let totals = conn.query_row(
        "SELECT
            COUNT(CASE WHEN scope='turn' THEN 1 END) AS turns,
            SUM(CASE WHEN scope='turn' THEN input_tokens END),
            SUM(CASE WHEN scope='turn' THEN output_tokens END),
            SUM(CASE WHEN scope='turn' THEN cache_read_tokens END),
            SUM(CASE WHEN scope='turn' THEN cache_write_tokens END),
            COUNT(CASE WHEN scope='session' THEN 1 END),
            COUNT(*),
            MAX(CASE WHEN scope='turn' AND observed_at_source='native' THEN observed_at END)
         FROM usage_events WHERE instance_id = ?1",
        params![instance_id],
        |row| {
            let turns: i64 = row.get(0)?;
            Ok(RollupTotals {
                turns,
                turn_input: row.get(1)?,
                turn_output: row.get(2)?,
                turn_cache_read: row.get(3)?,
                turn_cache_write: row.get(4)?,
                has_turn: turns > 0,
                has_session: row.get::<_, i64>(5)? > 0,
                last_turn_at: row.get(7)?,
            })
        },
    )?;
    if !totals.has_turn && !totals.has_session {
        return Ok(None);
    }

    // The cumulative stock exists only for session-emitting harnesses.
    let (stock, _stock_scoped) = if totals.has_session {
        latest_session_stock(conn, instance_id)?
    } else {
        (SessionStock::default(), false)
    };

    // Session totals: the authoritative cumulative stock when present, else the
    // sum of the per-turn flows.
    let (session_input, session_output, cache_read, cache_creation) = if totals.has_session {
        (
            stock.input,
            stock.output,
            stock.cache_read,
            stock.cache_write,
        )
    } else {
        (
            totals.turn_input,
            totals.turn_output,
            totals.turn_cache_read,
            totals.turn_cache_write,
        )
    };

    // Context basket: the newest turn call; for a stock-only harness the
    // stock's current counters.
    let context_used_tokens = if totals.has_turn {
        latest_turn_basket(conn, instance_id)?
    } else {
        [stock.input, stock.cache_read, stock.cache_write]
            .into_iter()
            .flatten()
            .reduce(i64::saturating_add)
    };

    let context_window_tokens = context_window_tokens(kind, model);
    let context_pct = context_used_tokens
        .zip(context_window_tokens)
        .map(|(used, window)| {
            (used as f64 / window as f64 * 100.0)
                .round()
                .clamp(0.0, 100.0) as i64
        });

    // Rate windows: per-turn flows for harnesses that emit them; otherwise
    // timestamped increments between cumulative session snapshots (r4 item 6).
    // A single stock with no predecessor yields no trustworthy rate.
    let window = |seconds: i64| -> rusqlite::Result<(Option<i64>, Option<i64>)> {
        let since = threshold_rfc3339(seconds);
        if totals.has_turn {
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
        context_used_tokens,
        context_window_tokens,
        context_pct,
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
                "payload": {
                    "scope": "turn",
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
        // Rollup still works after upgrade.
        assert!(
            rollup_instance(&conn, "ins_old", "claude", Some("fake"))
                .unwrap()
                .is_some()
        );
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
            scope_id: None,
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
        let rollup = rollup_instance(&conn, "ins_g", "grok", Some("grok-x"))
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
        };
        let cumulative = |seq: i64, input: i64, output: i64| UsageEventRow {
            scope: "session".into(),
            scope_id: Some("codex-session".into()),
            metric_revision: seq,
            ..turn(seq + 10, input, output)
        };
        insert_usage_event(&conn, &turn(1, 100, 10)).unwrap();
        insert_usage_event(&conn, &cumulative(1, 100, 10)).unwrap();
        insert_usage_event(&conn, &turn(2, 50, 5)).unwrap();
        insert_usage_event(&conn, &cumulative(2, 150, 15)).unwrap();

        let rollup = rollup_instance(&conn, "ins_c", "codex", Some("codex"))
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

        let rollup = rollup_instance(&conn, "ins_test", "claude", None)
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

        let rollup = rollup_instance(&conn, "ins_test", "grok", None)
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
        let rollup = rollup_instance(&conn, "ins_test", "generic", None)
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
            rollup_instance(&conn, "ins_empty", "claude", None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn context_window_resolution_order() {
        // Explicit [1m] tag wins over everything.
        assert_eq!(
            context_window_tokens("claude", Some("gw/model_x[1m]")),
            Some(1_000_000)
        );
        // Static catalog row for a real model.
        assert!(context_window_tokens("claude", Some("claude-opus-5")).is_some());
        // Kind fallback when the model is unknown.
        assert_eq!(
            context_window_tokens("grok", Some("mystery-model")),
            Some(128_000)
        );
        assert_eq!(context_window_tokens("claude", None), Some(200_000));
        // Nothing to say for a generic PTY.
        assert_eq!(context_window_tokens("generic", None), None);
    }

    // --- c-ctxusage r3 -----------------------------------------------------

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

        let rollup = rollup_instance(&conn, "ins_mix", "codex", None)
            .unwrap()
            .unwrap();
        assert_eq!(
            rollup.session_input_tokens,
            Some(300),
            "authoritative cumulative stock"
        );
        assert_eq!(rollup.session_output_tokens, Some(120));
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
        let rollup = rollup_instance(&conn, "ins_grok", "grok", None)
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
        let rollup = rollup_instance(&conn, "ins_grok", "grok", None)
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
        let rollup = rollup_instance(&conn, "ins_grok", "grok", None)
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
        let rollup = rollup_instance(&conn, &inst_str, "claude", Some("m"))
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
        let rollup = rollup_instance(&conn, "ins_r4", "claude", None)
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
        let rollup = rollup_instance(&conn, "ins_r5", "claude", None)
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

        let rollup = rollup_instance(&conn, "ins_r5", "claude", None)
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

    /// c-ctxusage r11 item 5: a native replay with identical counters whose
    /// native timestamp string is EQUAL to the stored ingest timestamp must
    /// still repair provenance - the source changes ingest -> native. The old
    /// inequality requirement meant a native observation carrying exactly the
    /// fallback string left the row falsely marked ingest.
    #[test]
    fn an_equal_timestamp_native_replay_still_repairs_provenance() {
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

        // Native replay: identical counters, native timestamp EQUAL to the
        // stored ingest fallback string.
        let at = "2026-09-15T00:00:00.000Z";
        let repaired = scoped_record(
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
            insert(&conn, &repaired),
            "an equal-timestamp native replay is a provenance repair, not a freeze"
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
        assert_eq!(
            source, "native",
            "provenance repaired despite equal strings"
        );
        assert_eq!(seq, 2, "the repair adopts the replay's seq");

        // A subsequent identical NATIVE replay is a genuine no-op.
        let again = scoped_record(
            3,
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
            !insert(&conn, &again),
            "native -> native with identical counters/time changes nothing"
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

    /// The current scope_id stamped on an instance's session row by seq.
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
        let rollup = rollup_instance(&conn, "ins_3a", "codex", Some("codex"))
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
        assert!(insert(&conn, &current), "growth above the floor appends");
        let rollup = rollup_instance(&conn, "ins_3b", "codex", Some("codex"))
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
        let rollup = rollup_instance(&conn, "ins_8", "codex", Some("codex"))
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
        let rollup = rollup_instance(&conn, "ins_5b", "codex", Some("codex"))
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

        let rollup = rollup_instance(&conn, "ins_5b", "codex", Some("codex"))
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
        let rollup = rollup_instance(&conn, "ins_r10", "codex", Some("codex"))
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
        let rollup = rollup_instance(&conn, "ins_r11", "codex", Some("codex"))
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
        let rollup = rollup_instance(&conn, "ins_r11", "codex", Some("codex"))
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

        let rollup = rollup_instance(&conn, "ins_r57", "codex", None)
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
        let rollup = rollup_instance(&conn, "ins_r57", "codex", None)
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
        let rollup = rollup_instance(&conn, "ins_live", "claude", None)
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
        let rollup = rollup_instance(&conn, "ins_live", "claude", None)
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

        let rollup = rollup_instance(&conn, "ins_9", "grok", None)
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
        let rollup = rollup_instance(&conn, "ins_9", "grok", None)
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
        // snapshot at the same turn end (native time, per-call flow).
        assert!(insert(
            &conn,
            &scoped_record(
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
        let rollup = rollup_instance(&conn, "ins_cx", "codex", Some("codex"))
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

        let rollup = rollup_instance(&conn, "ins_o3", "grok", None)
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

        let rollup = rollup_instance(&conn, "ins_o4", "grok", None)
            .unwrap()
            .unwrap();
        // The cumulative stock itself is still reported at 150.
        assert_eq!(rollup.session_input_tokens, Some(150));
        assert_eq!(rollup.session_output_tokens, Some(15));
        assert_eq!(rollup.tpm_in_60s, None);
        assert_eq!(rollup.tpm_out_60s, Some(5));
    }
}
