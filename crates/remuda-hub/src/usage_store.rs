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
    /// Native per-model context window the harness reported
    /// (`UsagePayload.contextWindow`, e.g. stream-json
    /// `modelUsage.<model>.contextWindow`), when it reported one.
    pub native_context_window: Option<i64>,
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
            native_context_window INTEGER,
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
    // r2: snapshot revision for revision-aware Session upserts (default 1).
    crate::store::ensure_column(conn, "usage_events", "metric_revision", "INTEGER")?;
    // c-usagefu: native-reported per-model context window.
    crate::store::ensure_column(conn, "usage_events", "native_context_window", "INTEGER")?;
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
/// Dedupe semantics by scope:
/// - **Turn** observations (Claude message id) and rows with no scope_id are
///   immutable: `INSERT OR IGNORE` on `(instance_id, seq)` / the partial
///   `(instance_id, scope, scope_id)` index, so re-hydration never changes a
///   completed turn.
/// - **Session** snapshots (Grok/Codex keep a stable session id and re-emit as
///   `usage.json` changes) are revision-aware: an equal-or-older
///   `metric_revision` is frozen, a newer one replaces the row in place.
pub fn insert_usage_event(conn: &Connection, row: &UsageEventRow) -> rusqlite::Result<bool> {
    if row.scope == "session" && row.scope_id.is_some() {
        upsert_session_snapshot(conn, row)
    } else {
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO usage_events
                (instance_id, seq, profile_id, model, scope, scope_id, mode,
                 metric_revision, total_tokens, input_tokens, output_tokens,
                 cache_read_tokens, cache_write_tokens, cost_usd, accounting,
                 native_context_window, observed_at)
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
                row.native_context_window,
                row.observed_at,
            ],
        )?;
        Ok(inserted > 0)
    }
}

/// Insert or replace a `scope='session'` snapshot keyed on the stable session
/// id, but only when the incoming revision is newer. Returns true when a row
/// was inserted or updated.
fn upsert_session_snapshot(conn: &Connection, row: &UsageEventRow) -> rusqlite::Result<bool> {
    let changed = conn.execute(
        "INSERT INTO usage_events
            (instance_id, seq, profile_id, model, scope, scope_id, mode,
             metric_revision, total_tokens, input_tokens, output_tokens,
             cache_read_tokens, cache_write_tokens, cost_usd, accounting,
             native_context_window, observed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
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
            native_context_window = excluded.native_context_window,
            observed_at = excluded.observed_at
         WHERE excluded.metric_revision > usage_events.metric_revision",
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
            row.native_context_window,
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

/// Parse a bare U64 scalar (decimal string on the wire, accepting bare
/// integers from looser producers).
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
        metric_revision: payload
            .get("metricRevision")
            .and_then(scalar_u64)
            .unwrap_or(1),
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
        // c-usagefu (a): native per-model window (U64 wire string, but accept
        // bare integers from older/looser producers too).
        native_context_window: payload.get("contextWindow").and_then(scalar_u64),
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
    /// True when `context_window_tokens` came from the harness-kind fallback
    /// (no native report, no effective/profile/catalog evidence). The chip
    /// paints `≈%`; false for a window the model or harness actually stated.
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

/// Kind-level fallback context windows, matching the web's own table before
/// the model catalog was available. Generic/terminal kinds report nothing.
fn kind_context_window(kind: &str) -> Option<i64> {
    match kind {
        "claude" | "codex" | "agy" => Some(200_000),
        "grok" => Some(128_000),
        _ => None,
    }
}

/// True for a model id carrying the explicit long-context tag, regardless of
/// catalog knowledge (`<gateway-model>[1m]` is not in the static table).
fn is_one_million_tag(model: &str) -> bool {
    model.trim().to_ascii_lowercase().ends_with("[1m]")
}

/// Exact evidence from one model id: the `[1m]` tag wins, then the static
/// capability catalog.
fn window_from_model_id(model: &str) -> Option<i64> {
    let model = model.trim();
    if model.is_empty() {
        return None;
    }
    if is_one_million_tag(model) {
        return Some(1_000_000);
    }
    crate::model_catalog::lookup(model).map(|row| row.context_window as i64)
}

/// A context window declared on the provider profile's model catalog for the
/// given wire id (exact match; a `[1m]` spelling retries without the tag).
fn profile_context_window(
    conn: &Connection,
    profile_id: &str,
    model_id: &str,
) -> rusqlite::Result<Option<i64>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT models_json FROM provider_profiles WHERE id = ?1",
            params![profile_id],
            |row| row.get(0),
        )
        .ok();
    let Some(raw) = raw else {
        return Ok(None);
    };
    let models = crate::provider_models::parse_models_json(&raw);
    let wanted = model_id.trim();
    let wanted_no_tag = wanted.strip_suffix("[1m]").unwrap_or(wanted);
    Ok(models
        .iter()
        .filter(|entry| entry.id == wanted || entry.id == wanted_no_tag)
        .find_map(|entry| entry.context_window.map(i64::try_from).and_then(Result::ok)))
}

/// Resolve a session's context window and whether the value is only an
/// estimate.
///
/// Order (c-usagefu (c)): native usage-reported window (the CLI's own
/// `modelUsage.contextWindow`, exact) → effective model id after a `/model`
/// switch (`[1m]` tag, then static catalog) → the provider profile's declared
/// `contextWindow` (effective id, then launch model) → static catalog on the
/// launch model → harness-kind fallback. Only the last step is approximate;
/// the chip paints `≈` for it and an unknown generic kind reports no window.
#[must_use]
fn resolve_context_window(
    conn: &Connection,
    req: &RollupRequest<'_>,
    native_window: Option<i64>,
) -> (Option<i64>, bool) {
    if let Some(window) = native_window {
        return (Some(window), false);
    }
    if let Some(window) = req.effective_model.and_then(window_from_model_id) {
        return (Some(window), false);
    }
    if let Some(profile_id) = req.profile_id {
        for candidate in [req.effective_model, req.spec_model].into_iter().flatten() {
            if let Ok(Some(window)) = profile_context_window(conn, profile_id, candidate) {
                return (Some(window), false);
            }
        }
    }
    if let Some(window) = req.spec_model.and_then(window_from_model_id) {
        return (Some(window), false);
    }
    match kind_context_window(req.kind) {
        Some(window) => (Some(window), true),
        // A generic/terminal kind with an unknown model has no window at all;
        // "approximate" is meaningless without a percentage.
        None => (None, false),
    }
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

fn sum_tokens_since(
    conn: &Connection,
    instance_id: &str,
    since: &str,
    scope: &str,
) -> rusqlite::Result<(Option<i64>, Option<i64>)> {
    conn.query_row(
        "SELECT SUM(input_tokens), SUM(output_tokens)
         FROM usage_events
         WHERE instance_id = ?1 AND observed_at >= ?2 AND scope = ?3",
        params![instance_id, since, scope],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
}

/// Inputs to [`rollup_instance`] beyond the connection: what the Hub knows
/// about the session's current model. The rollup is folded on every read, so
/// a `modelEffective` change is picked up with no invalidation.
pub struct RollupRequest<'a> {
    pub instance_id: &'a str,
    pub kind: &'a str,
    /// Launch-pinned model (`instances.model`).
    pub spec_model: Option<&'a str>,
    /// Transcript-observed effective model id (`spec.modelEffective.id`),
    /// i.e. the model a `/model` switch actually selected.
    pub effective_model: Option<&'a str>,
    pub profile_id: Option<&'a str>,
}

/// Session-wide totals of one rollup query (named so the fold signature
/// stays below the type-complexity lint).
struct RollupTotals {
    turns: i64,
    /// Additive sums over scope='turn' rows.
    turn_input: Option<i64>,
    turn_output: Option<i64>,
    turn_cache_read: Option<i64>,
    turn_cache_write: Option<i64>,
    /// Totals over scope='session' rows (harnesses that emit only session
    /// snapshots, e.g. Grok). Used only when there are no turn rows.
    sess_input: Option<i64>,
    sess_output: Option<i64>,
    sess_cache_read: Option<i64>,
    sess_cache_write: Option<i64>,
    has_any: bool,
    last_turn_at: Option<String>,
}

/// One bucket of the turn-vs-session coverage check: a cumulative Session
/// value is covered by the turn rows only when it reports the same total.
/// A bucket the session snapshot does not report imposes no constraint.
fn bucket_covers(turn_sum: Option<i64>, session_total: Option<i64>) -> bool {
    match session_total {
        None => true,
        Some(total) => turn_sum == Some(total),
    }
}

/// Fold every persisted usage event of one instance into
/// [`InstanceUsageRollup`]. Returns `None` when the session has no usage
/// observations yet, so the field stays off the instance record entirely.
///
/// Turn and Session scopes are kept separate (c-ctxusage r2): Codex emits both
/// per-turn additive rows and a cumulative session snapshot, so summing both
/// double-counts. c-usagefu (b) decides which is authoritative:
/// - no turn rows → the cumulative Session snapshot (Grok);
/// - turn rows and no cumulative token snapshot → the turn rows (Claude, and
///   print whose `result` rows carry cost only);
/// - both → turn rows only when they COVER the cumulative total in every
///   bucket the snapshot reports (a tail bound mid-session never does),
///   otherwise the authoritative cumulative Session total.
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
            SUM(CASE WHEN scope='session' THEN input_tokens END),
            SUM(CASE WHEN scope='session' THEN output_tokens END),
            SUM(CASE WHEN scope='session' THEN cache_read_tokens END),
            SUM(CASE WHEN scope='session' THEN cache_write_tokens END),
            COUNT(*),
            MAX(CASE WHEN scope='turn' THEN observed_at END)
         FROM usage_events WHERE instance_id = ?1",
        params![instance_id],
        |row| {
            Ok(RollupTotals {
                turns: row.get(0)?,
                turn_input: row.get(1)?,
                turn_output: row.get(2)?,
                turn_cache_read: row.get(3)?,
                turn_cache_write: row.get(4)?,
                sess_input: row.get(5)?,
                sess_output: row.get(6)?,
                sess_cache_read: row.get(7)?,
                sess_cache_write: row.get(8)?,
                has_any: row.get::<_, i64>(9)? > 0,
                last_turn_at: row.get(10)?,
            })
        },
    )?;
    if !totals.has_any {
        return Ok(None);
    }
    let session_reports_tokens = [
        totals.sess_input,
        totals.sess_output,
        totals.sess_cache_read,
        totals.sess_cache_write,
    ]
    .iter()
    .any(Option::is_some);
    let use_turn = if totals.turns == 0 {
        false
    } else if !session_reports_tokens {
        true
    } else {
        bucket_covers(totals.turn_input, totals.sess_input)
            && bucket_covers(totals.turn_output, totals.sess_output)
            && bucket_covers(totals.turn_cache_read, totals.sess_cache_read)
            && bucket_covers(totals.turn_cache_write, totals.sess_cache_write)
    };
    let authoritative_scope = if use_turn { "turn" } else { "session" };
    let session_input = if use_turn {
        totals.turn_input
    } else {
        totals.sess_input
    };
    let session_output = if use_turn {
        totals.turn_output
    } else {
        totals.sess_output
    };
    let cache_read = if use_turn {
        totals.turn_cache_read
    } else {
        totals.sess_cache_read
    };
    let cache_creation = if use_turn {
        totals.turn_cache_write
    } else {
        totals.sess_cache_write
    };

    // What the next request carries comes from the NEWEST per-REQUEST row:
    // one model response's fresh input plus every cached bucket, summed over
    // whichever buckets that response actually reported. Codex emits Message
    // rows (one per response_id) because its Turn rows sum multiple responses
    // in a turn; Claude's Turn rows are themselves per-request; Grok gives
    // only cumulative Session rows. Scope preference makes a later
    // end-of-turn summary never mask the latest single response. The same
    // row's native window is preferred; a later cost-only row may still
    // carry one, so fall back to the newest reported window of any scope.
    let (context_used_tokens, row_native_window) = conn
        .query_row(
            "SELECT input_tokens, cache_read_tokens, cache_write_tokens, native_context_window
             FROM usage_events
             WHERE instance_id = ?1
               AND scope IN ('message', 'turn', 'session')
             ORDER BY CASE scope
                        WHEN 'message' THEN 0
                        WHEN 'turn' THEN 1
                        ELSE 2 END,
                      seq DESC
             LIMIT 1",
            params![instance_id],
            |row| {
                let input: Option<i64> = row.get(0)?;
                let cache_read: Option<i64> = row.get(1)?;
                let cache_write: Option<i64> = row.get(2)?;
                let window: Option<i64> = row.get(3)?;
                let components = [input, cache_read, cache_write].into_iter().flatten();
                Ok::<_, rusqlite::Error>((components.reduce(i64::saturating_add), window))
            },
        )
        .unwrap_or((None, None));
    let native_window = row_native_window.or_else(|| {
        conn.query_row(
            "SELECT native_context_window FROM usage_events
             WHERE instance_id = ?1 AND native_context_window IS NOT NULL
             ORDER BY seq DESC LIMIT 1",
            params![instance_id],
            |row| row.get(0),
        )
        .ok()
        .flatten()
    });

    let (context_window_tokens, window_approximate) =
        resolve_context_window(conn, req, native_window);
    let context_pct = context_used_tokens
        .zip(context_window_tokens)
        .map(|(used, window)| {
            (used as f64 / window as f64 * 100.0)
                .round()
                .clamp(0.0, 100.0) as i64
        });
    // Without a window there is no percentage, so there is nothing to mark
    // approximate.
    let context_pct_approximate = context_pct.is_some() && window_approximate;

    let (in_60s, out_60s) =
        sum_tokens_since(conn, instance_id, &threshold_rfc3339(60), authoritative_scope)?;
    let (in_5m, out_5m) =
        sum_tokens_since(conn, instance_id, &threshold_rfc3339(300), authoritative_scope)?;
    // The 5-minute figure is an average per-minute rate (sum / 5).
    let per_minute_5m =
        |total: Option<i64>| -> Option<i64> { total.map(|n| (n as f64 / 5.0).round() as i64) };

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
        last_turn_at: totals.last_turn_at,
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
            fold_rollup(&conn, "ins_old", "claude", Some("fake"))
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
            native_context_window: None,
            observed_at: observed_at.into(),
        }
    }

    /// Rollup with no effective-model / profile inputs (the static catalog
    /// and kind fallback path).
    fn fold_rollup(
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
            native_context_window: None,
            observed_at: format!("2026-10-06T00:00:0{seq}.000Z"),
        };
        // Revision 1, then 2 (replaces), then a stale 1 re-delivery (frozen).
        assert!(insert_usage_event(&conn, &sess(1, 1, 100, 10)).unwrap());
        assert!(insert_usage_event(&conn, &sess(2, 2, 200, 20)).unwrap());
        assert!(!insert_usage_event(&conn, &sess(3, 1, 999, 999)).unwrap());

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_events WHERE instance_id='ins_g'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "one replaced session row, not three");
        let (rev, input, output): (i64, i64, i64) = conn
            .query_row(
                "SELECT metric_revision, input_tokens, output_tokens
                 FROM usage_events WHERE instance_id='ins_g'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((rev, input, output), (2, 200, 20), "newest revision wins");
        // Session-only totals roll up from the session row (no turn rows).
        let rollup = fold_rollup(&conn, "ins_g", "grok", Some("grok-x"))
            .unwrap()
            .unwrap();
        assert_eq!(rollup.turns, 0, "session snapshots are not turns");
        assert_eq!(rollup.session_input_tokens, Some(200));
        assert_eq!(rollup.session_output_tokens, Some(20));
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
            native_context_window: None,
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

        let rollup = fold_rollup(&conn, "ins_c", "codex", Some("codex"))
            .unwrap()
            .unwrap();
        assert_eq!(rollup.turns, 2);
        assert_eq!(
            rollup.session_input_tokens,
            Some(150),
            "turns only, no session"
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

        let rollup = fold_rollup(&conn, "ins_test", "claude", None)
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

        let rollup = fold_rollup(&conn, "ins_test", "grok", None)
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
        let rollup = fold_rollup(&conn, "ins_test", "generic", None)
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
            fold_rollup(&conn, "ins_empty", "claude", None)
                .unwrap()
                .is_none()
        );
    }

    fn rollup_request<'a>(
        instance_id: &'a str,
        kind: &'a str,
        spec_model: Option<&'a str>,
        effective_model: Option<&'a str>,
        profile_id: Option<&'a str>,
    ) -> RollupRequest<'a> {
        RollupRequest {
            instance_id,
            kind,
            spec_model,
            effective_model,
            profile_id,
        }
    }

    #[test]
    fn context_window_resolution_order() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let req = |spec: Option<&'static str>, eff: Option<&'static str>| {
            rollup_request("ins_x", "claude", spec, eff, None)
        };

        // Explicit [1m] tag on the effective id wins, even for a gateway
        // model the static catalog has never heard of. Not approximate.
        assert_eq!(
            resolve_context_window(
                &conn,
                &req(Some("passthrough/ark/seed-evolving"), Some("passthrough/ark/seed-evolving[1m]")),
                None
            ),
            (Some(1_000_000), false)
        );
        // A catalog row for the effective model.
        assert_eq!(
            resolve_context_window(
                &conn,
                &req(Some("claude-haiku-4-5"), Some("claude-opus-5")),
                None
            ),
            (Some(1_048_576), false)
        );
        // Static catalog on the launch model.
        assert_eq!(
            resolve_context_window(&conn, &req(Some("claude-opus-5"), None), None),
            (Some(1_048_576), false)
        );
        // Unknown model keeps the harness-kind fallback — and is approximate.
        assert_eq!(
            resolve_context_window(&conn, &req(Some("mystery-model"), None), None),
            (Some(200_000), true)
        );
        // No model at all: kind fallback, approximate.
        assert_eq!(
            resolve_context_window(&conn, &req(None, None), None),
            (Some(200_000), true)
        );
        // Nothing to say for a generic PTY with an unknown model.
        assert_eq!(
            resolve_context_window(
                &conn,
                &rollup_request("ins_x", "generic", Some("mystery"), None, None),
                None
            ),
            (None, false)
        );
        // The native-reported window beats every other source.
        assert_eq!(
            resolve_context_window(
                &conn,
                &req(Some("mystery-model"), Some("mystery-model[1m]")),
                Some(123_456)
            ),
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
            "INSERT INTO provider_profiles (id, models_json) VALUES (?1, ?2)",
            params![
                "pvp_test",
                r#"[{"id":"gw/model-x","contextWindow":250000,"enabled":true}]"#
            ],
        )
        .unwrap();

        let req = rollup_request(
            "ins_x",
            "claude",
            Some("gw/model-x"),
            Some("gw/model-x"),
            Some("pvp_test"),
        );
        assert_eq!(
            resolve_context_window(&conn, &req, None),
            (Some(250_000), false),
            "profile-declared window is evidence, not an estimate"
        );
        // The effective id is checked before the launch model: a switch to
        // the profile's 1m spelling wins through the profile too.
        conn.execute(
            "UPDATE provider_profiles SET models_json = ?1 WHERE id = 'pvp_test'",
            params![r#"[{"id":"gw/model-x[1m]","contextWindow":1000000,"enabled":true}]"#],
        )
        .unwrap();
        let req = rollup_request(
            "ins_x",
            "claude",
            Some("gw/model-x"),
            Some("gw/model-x[1m]"),
            Some("pvp_test"),
        );
        assert_eq!(
            resolve_context_window(&conn, &req, None),
            (Some(1_000_000), false)
        );
    }

    /// c-usagefu (b): the REAL 0.154.0 Codex rollout, replayed through the
    /// driver's `CodexAdapter` and projected into the Hub exactly as the
    /// `journal.append` path does. Six turns, ten model responses, each with
    /// input=100/cached=10 (uncached 90): the rollup must fold the additive
    /// turn rows to 900 uncached input (the cumulative session snapshot alone
    /// agrees, so turn coverage is complete), and the context figure must come
    /// from the last per-request MESSAGE row (90 fresh + 10 read = 100),
    /// never the summed turn row (200) or the cumulative session row (1000).
    #[test]
    fn codex_fixture_replay_rolls_up_six_turns_and_900_uncached_input() {
        use remuda_driver::adapters::{AdapterHome, CodexAdapter, FileSignalAdapter};

        const ROLLOUT: &str =
            include_str!("../../remuda-driver/tests/fixtures/codex/interactive-0.154.0.jsonl");
        let session = "01a09c00-64d4-7ce1-8537-984e896b8e8a";

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        std::fs::write(
            &path,
            format!(
                "{{\"ordinal\":0,\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session}\",\"session_id\":\"{session}\"}}}}\n{ROLLOUT}"
            ),
        )
        .unwrap();

        let mut adapter = CodexAdapter::new(AdapterHome {
            home: dir.path().to_path_buf(),
            cwd: dir.path().to_path_buf(),
            pid: None,
        });
        adapter.bind_rollout(session, path);
        let observations = adapter.poll().unwrap();

        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let mut seq = 0i64;
        for observed in observations {
            let remuda_protocol::ObservationPayload::Usage(payload) = observed.payload else {
                continue;
            };
            seq += 1;
            let record = JournalRecord {
                instance_id: "ins_codex".into(),
                seq,
                event_id: format!("evt_{seq}"),
                event: serde_json::json!({
                    "kind": "usage",
                    "payload": serde_json::to_value(&payload).unwrap(),
                }),
                observed_at: format!("2026-09-13T18:21:{:02}.000Z", 40 + (seq % 10)),
            };
            let row = project_usage_event(&record, None, Some("gpt-5.4")).expect("usage row");
            assert!(insert_usage_event(&conn, &row).unwrap());
        }
        assert!(seq >= 6, "the fixture emits turn/session usage rows: {seq}");

        let rollup = rollup_instance(
            &conn,
            &rollup_request("ins_codex", "codex", Some("gpt-5.4"), None, None),
        )
        .unwrap()
        .unwrap();
        assert_eq!(rollup.turns, 6, "six end-of-turn snapshots");
        assert_eq!(
            rollup.session_input_tokens,
            Some(900),
            "uncached input: ten responses × (100 - 10 cached)"
        );
        assert_eq!(
            rollup.context_used_tokens,
            Some(100),
            "context comes from the last per-request row, not the 1000-token cumulative session row"
        );
    }

    #[test]
    fn one_million_switch_refreshes_the_rollup_window() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        insert_usage_event(
            &conn,
            &row(
                1,
                &ago(2),
                Some(50_000),
                Some(100),
                Some(100_000),
                Some(0),
            ),
        )
        .unwrap();

        // Before the switch: unknown gateway model, 200k kind fallback.
        let before = rollup_instance(
            &conn,
            &rollup_request(
                "ins_test",
                "claude",
                Some("passthrough/ark/seed-evolving"),
                None,
                None,
            ),
        )
        .unwrap()
        .unwrap();
        assert_eq!(before.context_window_tokens, Some(200_000));
        assert!(before.context_pct_approximate, "fallback is approximate");
        assert_eq!(before.context_pct, Some(75));

        // After a /model switch to the 1m spelling: window flips to 1,000,000
        // and the percentage recomputes (same folded rows; rollup is on read).
        let after = rollup_instance(
            &conn,
            &rollup_request(
                "ins_test",
                "claude",
                Some("passthrough/ark/seed-evolving"),
                Some("passthrough/ark/seed-evolving[1m]"),
                None,
            ),
        )
        .unwrap()
        .unwrap();
        assert_eq!(after.context_window_tokens, Some(1_000_000));
        assert!(!after.context_pct_approximate, "[1m] tag is evidence");
        assert_eq!(after.context_pct, Some(15));
    }
}
