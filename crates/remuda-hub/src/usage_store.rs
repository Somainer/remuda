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
    conn.execute_batch(
        "DROP INDEX IF EXISTS usage_events_scope_dedupe;
         CREATE UNIQUE INDEX IF NOT EXISTS usage_events_turn_dedupe
             ON usage_events(instance_id, scope, scope_id)
             WHERE scope = 'turn' AND scope_id IS NOT NULL;",
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
/// backwards against: the point's OWN session id plus any LEGACY NULL-scope
/// session rows (c-ctxusage r5 item 3B). A byte-0 restart replay cannot
/// re-append a smaller stock than the old-format cumulative total.
fn existing_session_floors(
    conn: &Connection,
    row: &UsageEventRow,
) -> rusqlite::Result<Vec<SnapshotCounters>> {
    let mut stmt = conn.prepare(
        "SELECT total_tokens, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens
         FROM usage_events
         WHERE instance_id = ?1 AND scope = 'session'
           AND (scope_id = ?2 OR scope_id IS NULL)",
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
/// historical/duplicate replays (c-ctxusage r4 item 3 / r5 item 3B).
///
/// The cumulative stock is monotonic by contract, so acceptance is content-
/// based and independent of the restart-unsafe producer revision and ingest
/// seq:
/// - identical counters already recorded for this session (or for a legacy
///   NULL-scope row) -> frozen;
/// - any reported bucket smaller than the current stock -> frozen;
/// - otherwise the point is appended.
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
             observed_at, observed_at_source, native_context_window)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
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
            row.native_context_window,
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

fn static_window_from_model_id(model: &str) -> Option<i64> {
    let model = model.trim();
    if is_one_million_tag(model) {
        return Some(1_000_000);
    }
    if let Some(row) = crate::model_catalog::lookup(model) {
        return Some(row.context_window as i64);
    }
    None
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
/// `modelUsage`) wins; then each model candidate — transcript-observed
/// effective id first, then the launch model — through its `[1m]` tag/static
/// catalog row and its provider profile's declared `contextWindow`; finally the
/// harness-kind fallback. Only the last step is approximate (a guess at an
/// unknown gateway model's window).
fn resolve_context_window(
    conn: &Connection,
    req: &RollupRequest<'_>,
    native_window: Option<i64>,
) -> (Option<i64>, bool) {
    if let Some(window) = native_window {
        return (Some(window), false);
    }
    for (model, profile_id) in [
        (req.effective_model, req.profile_id),
        (req.spec_model, req.profile_id),
    ] {
        let Some(model) = model else {
            continue;
        };
        if let Some(window) = static_window_from_model_id(model) {
            return (Some(window), false);
        }
        if let Some(profile_id) = profile_id
            && let Ok(Some(window)) = profile_context_window(conn, profile_id, model)
        {
            return (Some(window), false);
        }
    }
    match kind_context_window(req.kind) {
        Some(window) => (Some(window), true),
        None => (None, false),
    }
}

/// Session-wide totals of one rollup query.
struct RollupTotals {
    turns: i64,
    /// Additive sums over scope='turn' rows; used for totals when they cover
    /// the cumulative session stock (or when no session stock exists).
    turn_input: Option<i64>,
    turn_output: Option<i64>,
    turn_cache_read: Option<i64>,
    turn_cache_write: Option<i64>,
    has_turn: bool,
    has_session: bool,
    last_turn_at: Option<String>,
}

/// Per-bucket coverage: do the additive turn sums cover (reach) every bucket
/// the authoritative cumulative session snapshot reports? `None` incoming
/// imposes no constraint. A turn total below the session stock means turn
/// coverage is incomplete (a tail bound mid-session / interrupted replay), so
/// the cumulative session total must be used instead (c-usagefu (b)).
fn bucket_covers(turn: Option<i64>, session: Option<i64>) -> bool {
    match (turn, session) {
        (_, None) => true,
        (Some(turn), Some(session)) => turn >= session,
        (None, Some(_)) => false,
    }
}

/// Fold every persisted usage event of one instance into
/// [`InstanceUsageRollup`]. Returns `None` when the session has no usage
/// observations yet, so the field stays off the instance record entirely.
///
/// Stock vs flow:
/// - A **Session** row is an append-only cumulative STOCK growth point
///   (Grok/Codex); the latest accepted point is the current stock.
/// - **Turn** rows are per-turn FLOW (Claude emits only those); **Message**
///   rows are per-response flow (Codex).
///
/// Totals come from the turn sums when they COVER the session stock and from
/// the latest stock otherwise (c-usagefu (b)): an interrupted replay must not
/// let a partial turn sum replace the authoritative cumulative total. The
/// context basket is the newest message/turn call, falling back to the stock.
/// Rate windows sum turn flows; a stock-only harness contributes timestamped
/// increments between successive growth points (never the whole stock).
pub fn rollup_instance(
    conn: &Connection,
    req: &RollupRequest<'_>,
) -> rusqlite::Result<Option<InstanceUsageRollup>> {
    let instance_id = req.instance_id;
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

    // Totals: turn sums when they cover the authoritative stock in every
    // reported bucket (or when there is no stock), else the current stock.
    let use_turn = !totals.has_session
        || (bucket_covers(totals.turn_input, stock.input)
            && bucket_covers(totals.turn_output, stock.output)
            && bucket_covers(totals.turn_cache_read, stock.cache_read)
            && bucket_covers(totals.turn_cache_write, stock.cache_write));
    let (session_input, session_output, cache_read, cache_creation) = if use_turn {
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
            rollup_fold(&conn, "ins_old", "claude", Some("fake"))
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
