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
    // Now the column exists on both a fresh CREATE and an upgraded DB.
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS usage_events_scope_dedupe
             ON usage_events(instance_id, scope, scope_id)
             WHERE scope_id IS NOT NULL;",
    )?;
    Ok(())
}

/// Inserts a usage projection.
///
/// Dedupe semantics by whether the row carries a durable `scope_id`:
/// - Rows with no `scope_id` are append-only: plain `INSERT OR IGNORE` on
///   `(instance_id, seq)`.
/// - Every scoped row (a Claude **Turn** keyed by the assistant message id, and
///   a Grok/Codex **Session** keyed by the stable session id) is revision-aware:
///   a later-arriving row (`seq`) replaces the stored one when it carries a
///   higher `metric_revision` OR when its counters changed. Replacing on a
///   changed body with a reset/equal revision is what lets a producer whose
///   in-memory revision counter restarted at 0 (an adapter recreation) still
///   advance its snapshot, while an unchanged re-hydration is frozen.
///
/// Turn rows are therefore NOT keep-first: a message that streamed a stop record
/// at one poll and then a later same-message-id record with different counters
/// (c-ctxusage r3 item 1) replaces the provisional row with the final one.
pub fn insert_usage_event(conn: &Connection, row: &UsageEventRow) -> rusqlite::Result<bool> {
    if row.scope_id.is_some() {
        upsert_scoped_snapshot(conn, row)
    } else {
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO usage_events
                (instance_id, seq, profile_id, model, scope, scope_id, mode,
                 metric_revision, total_tokens, input_tokens, output_tokens,
                 cache_read_tokens, cache_write_tokens, cost_usd, accounting, observed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
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
            ],
        )?;
        Ok(inserted > 0)
    }
}

/// Insert or replace a scoped (`scope_id IS NOT NULL`) snapshot, but only when
/// the incoming row is genuinely newer. Returns true when a row was inserted or
/// updated.
///
/// Ordering is the durable per-instance journal `seq` (later = observed later
/// on the single ordered channel), NOT the producer's `metric_revision`: a
/// Grok/Codex adapter recreates and its in-memory revision counter restarts at
/// 0/1, so producer revisions are not monotonic across a run. A later row
/// replaces the stored one only when its token counters actually changed; an
/// unchanged re-delivery (a transcript re-hydration that appends fresh seqs) is
/// frozen even at a higher `seq`. The decoded `metric_revision` is still
/// recorded for protocol/diagnostic fidelity (c-ctxusage r3 item 2).
fn upsert_scoped_snapshot(conn: &Connection, row: &UsageEventRow) -> rusqlite::Result<bool> {
    let changed = conn.execute(
        "INSERT INTO usage_events
            (instance_id, seq, profile_id, model, scope, scope_id, mode,
             metric_revision, total_tokens, input_tokens, output_tokens,
             cache_read_tokens, cache_write_tokens, cost_usd, accounting, observed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
         ON CONFLICT(instance_id, scope, scope_id) WHERE scope_id IS NOT NULL
         DO UPDATE SET
            seq = excluded.seq,
            profile_id = excluded.profile_id,
            model = excluded.model,
            mode = excluded.mode,
            metric_revision = excluded.metric_revision,
            total_tokens = excluded.total_tokens,
            input_tokens = excluded.input_tokens,
            output_tokens = excluded.output_tokens,
            cache_read_tokens = excluded.cache_read_tokens,
            cache_write_tokens = excluded.cache_write_tokens,
            cost_usd = excluded.cost_usd,
            accounting = excluded.accounting,
            observed_at = excluded.observed_at
         WHERE excluded.seq > usage_events.seq
           AND (
             excluded.total_tokens    IS NOT usage_events.total_tokens
             OR excluded.input_tokens    IS NOT usage_events.input_tokens
             OR excluded.output_tokens   IS NOT usage_events.output_tokens
             OR excluded.cache_read_tokens  IS NOT usage_events.cache_read_tokens
             OR excluded.cache_write_tokens IS NOT usage_events.cache_write_tokens
           )",
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
        ],
    )?;
    Ok(changed > 0)
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
    // journal ingest time for frames that did not carry one.
    let native_at = record
        .event
        .get("nativeAt")
        .and_then(|at| at.get("value"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| record.observed_at.clone());
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
        observed_at: native_at,
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
/// so they are excluded here; the caller substitutes the single latest stock
/// only when the harness emits no turn rows at all (Grok) — c-ctxusage r3 item 6.
fn sum_turn_tokens_since(
    conn: &Connection,
    instance_id: &str,
    since: &str,
) -> rusqlite::Result<(Option<i64>, Option<i64>)> {
    conn.query_row(
        "SELECT SUM(input_tokens), SUM(output_tokens)
         FROM usage_events
         WHERE instance_id = ?1 AND scope = 'turn' AND observed_at >= ?2",
        params![instance_id, since],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
}

/// The latest scope='session' cumulative snapshot (revisions replace it in
/// place, so the highest `seq` is the current stock). A harness that emits no
/// session snapshots (Claude) yields all-`None` fields.
#[derive(Debug, Default, Clone)]
struct SessionStock {
    input: Option<i64>,
    output: Option<i64>,
    cache_read: Option<i64>,
    cache_write: Option<i64>,
    observed_at: Option<String>,
}

fn latest_session_stock(conn: &Connection, instance_id: &str) -> rusqlite::Result<SessionStock> {
    conn.query_row(
        "SELECT input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, observed_at
         FROM usage_events
         WHERE instance_id = ?1 AND scope = 'session'
         ORDER BY seq DESC LIMIT 1",
        params![instance_id],
        |row| {
            Ok(SessionStock {
                input: row.get(0)?,
                output: row.get(1)?,
                cache_read: row.get(2)?,
                cache_write: row.get(3)?,
                observed_at: row.get(4)?,
            })
        },
    )
}

/// The newest per-call (scope='turn') basket: fresh input plus every cached
/// bucket of the most recent model call — what the NEXT request will carry. A
/// cumulative session snapshot is the wrong source here (it spans the whole
/// session), so turn rows take precedence even when the session row is the
/// newest by seq (c-ctxusage r3 item 6).
fn latest_turn_basket(conn: &Connection, instance_id: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT input_tokens, cache_read_tokens, cache_write_tokens
         FROM usage_events
         WHERE instance_id = ?1 AND scope = 'turn'
         ORDER BY seq DESC LIMIT 1",
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
            MAX(CASE WHEN scope='turn' THEN observed_at END)
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
    let stock = if totals.has_session {
        latest_session_stock(conn, instance_id)?
    } else {
        SessionStock::default()
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

    // Rate windows: per-turn flows. A stock-only harness (Grok) contributes its
    // single cumulative snapshot only while that snapshot is inside the window.
    let window = |seconds: i64| -> rusqlite::Result<(Option<i64>, Option<i64>)> {
        if totals.has_turn {
            sum_turn_tokens_since(conn, instance_id, &threshold_rfc3339(seconds))
        } else {
            let since = threshold_rfc3339(seconds);
            let inside = stock
                .observed_at
                .as_deref()
                .is_some_and(|at| at >= since.as_str());
            Ok(if inside {
                (stock.input, stock.output)
            } else {
                (None, None)
            })
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
        // c-ctxusage r2: a stable session id with increasing revision replaces
        // the prior row; an equal/older revision is frozen. Turn + Session rows
        // are never added together.
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
        };
        // Revision 1, then 2 (replaces as the counters advance). Ordering is
        // durable seq+body (c-ctxusage r3), not the producer's revision: an
        // unchanged re-delivery at any revision is frozen, and a producer that
        // RESTARTS its revision counter (adapter recreation) still advances the
        // row when its cumulative snapshot grew.
        assert!(insert_usage_event(&conn, &sess(1, 1, 100, 10)).unwrap());
        assert!(insert_usage_event(&conn, &sess(2, 2, 200, 20)).unwrap());
        // An unchanged re-hydration (fresh seq, identical body) is frozen even
        // though it arrives later.
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
        assert_eq!(count, 1, "one replaced session row");
        let (rev, input, output): (i64, i64, i64) = conn
            .query_row(
                "SELECT metric_revision, input_tokens, output_tokens
                 FROM usage_events WHERE instance_id='ins_g'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((rev, input, output), (1, 300, 30), "restarted growth wins");
        // Session-only totals roll up from the session row (no turn rows).
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

    fn row_revision_and_input(conn: &Connection, scope_id: &str) -> (i64, Option<i64>) {
        conn.query_row(
            "SELECT metric_revision, input_tokens FROM usage_events
             WHERE instance_id='ins_mix' AND scope_id=?1",
            params![scope_id],
            |row| Ok((row.get(0).unwrap(), row.get(1).unwrap())),
        )
        .unwrap()
    }

    #[test]
    fn bare_scalar_metric_revision_decodes_and_replaces_then_survives_restart() {
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
        assert!(insert(&conn, &rev2), "revision 2 replaces 1");
        assert_eq!(row_revision_and_input(&conn, "s1"), (2, Some(20)));

        // Adapter recreation restarts the producer counter at 1, but the
        // re-read cumulative snapshot has advanced: the changed body + higher
        // seq still replace it.
        let restarted = scoped_record(3, "ins_mix", "session", Some("s1"), 1, 35, 9, 0, 0, None);
        assert!(
            insert(&conn, &restarted),
            "a restarted producer's changed snapshot advances despite the reset revision"
        );
        assert_eq!(row_revision_and_input(&conn, "s1"), (1, Some(35)));

        // An unchanged re-emit (re-hydration / any revision) at a later seq is
        // frozen.
        let same = scoped_record(4, "ins_mix", "session", Some("s1"), 1, 35, 9, 0, 0, None);
        assert!(!insert(&conn, &same), "unchanged body never replaces");
        assert_eq!(row_revision_and_input(&conn, "s1"), (1, Some(35)));
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
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT input_tokens FROM usage_events WHERE scope_id='grok-session-9'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            200
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

        // A newer snapshot now replaces the formerly-frozen session row.
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
                "SELECT input_tokens FROM usage_events WHERE scope_id='sess-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(input, 80, "upgraded session rows keep updating");
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
    fn a_session_only_harness_uses_the_stock_for_totals_context_and_rates() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let now = time::OffsetDateTime::now_utc();
        let recent = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            now.year(),
            u8::from(now.month()),
            now.day(),
            now.hour(),
            now.minute(),
            now.second(),
            now.millisecond()
        );
        assert!(insert(
            &conn,
            &scoped_record(
                1,
                "ins_grok",
                "session",
                Some("g1"),
                3,
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
            rollup.tpm_in_60s,
            Some(200),
            "the single stock counts while in-window"
        );
        assert_eq!(rollup.last_turn_at.as_deref(), Some(recent.as_str()));
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
}
