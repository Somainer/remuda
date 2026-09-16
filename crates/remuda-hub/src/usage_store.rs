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

/// Create the `usage_events` table. Idempotent.
pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS usage_events (
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
         CREATE INDEX IF NOT EXISTS usage_events_profile_model
             ON usage_events(profile_id, model);
         CREATE INDEX IF NOT EXISTS usage_events_instance ON usage_events(instance_id);",
    )?;
    // Rows created before the context rollup carried no cache counters.
    crate::store::ensure_column(conn, "usage_events", "cache_read_tokens", "INTEGER")?;
    crate::store::ensure_column(conn, "usage_events", "cache_write_tokens", "INTEGER")?;
    Ok(())
}

/// Inserts a usage projection from a journal event. Idempotent on
/// `(instance_id, seq)` — replays are ignored.
pub fn insert_usage_event(conn: &Connection, row: &UsageEventRow) -> rusqlite::Result<bool> {
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

fn knowledge_u64(value: &Value) -> Option<i64> {
    let inner = value.get("value")?;
    // U64 is serialized as a decimal string on the wire.
    if let Some(text) = inner.as_str() {
        return text.parse().ok();
    }
    inner.as_i64()
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
        observed_at: record.observed_at.clone(),
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

fn sum_tokens_since(
    conn: &Connection,
    instance_id: &str,
    since: &str,
) -> rusqlite::Result<(Option<i64>, Option<i64>)> {
    conn.query_row(
        "SELECT SUM(input_tokens), SUM(output_tokens)
         FROM usage_events WHERE instance_id = ?1 AND observed_at >= ?2",
        params![instance_id, since],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
}

/// Session-wide totals of one rollup query (named so the fold signature
/// stays below the type-complexity lint).
struct RollupTotals {
    turns: i64,
    session_input: Option<i64>,
    session_output: Option<i64>,
    cache_read: Option<i64>,
    cache_creation: Option<i64>,
    last_turn_at: Option<String>,
}

/// Fold every persisted usage event of one instance into
/// [`InstanceUsageRollup`]. Returns `None` when the session has no usage
/// observations yet, so the field stays off the instance record entirely.
pub fn rollup_instance(
    conn: &Connection,
    instance_id: &str,
    kind: &str,
    model: Option<&str>,
) -> rusqlite::Result<Option<InstanceUsageRollup>> {
    let totals = conn.query_row(
        "SELECT COUNT(*),
                SUM(input_tokens),
                SUM(output_tokens),
                SUM(cache_read_tokens),
                SUM(cache_write_tokens),
                MAX(observed_at)
         FROM usage_events WHERE instance_id = ?1",
        params![instance_id],
        |row| {
            Ok(RollupTotals {
                turns: row.get(0)?,
                session_input: row.get(1)?,
                session_output: row.get(2)?,
                cache_read: row.get(3)?,
                cache_creation: row.get(4)?,
                last_turn_at: row.get(5)?,
            })
        },
    )?;
    if totals.turns == 0 {
        return Ok(None);
    }

    // What the next request carries comes from the newest observation:
    // fresh input plus every cached bucket, summed over whichever buckets
    // that turn actually reported.
    let context_used_tokens = conn
        .query_row(
            "SELECT input_tokens, cache_read_tokens, cache_write_tokens
             FROM usage_events WHERE instance_id = ?1
             ORDER BY seq DESC LIMIT 1",
            params![instance_id],
            |row| {
                let input: Option<i64> = row.get(0)?;
                let cache_read: Option<i64> = row.get(1)?;
                let cache_write: Option<i64> = row.get(2)?;
                let components = [input, cache_read, cache_write].into_iter().flatten();
                Ok::<_, rusqlite::Error>(components.reduce(i64::saturating_add))
            },
        )
        .ok()
        .flatten();

    let context_window_tokens = context_window_tokens(kind, model);
    let context_pct = context_used_tokens
        .zip(context_window_tokens)
        .map(|(used, window)| {
            (used as f64 / window as f64 * 100.0)
                .round()
                .clamp(0.0, 100.0) as i64
        });

    let (in_60s, out_60s) = sum_tokens_since(conn, instance_id, &threshold_rfc3339(60))?;
    let (in_5m, out_5m) = sum_tokens_since(conn, instance_id, &threshold_rfc3339(300))?;
    // The 5-minute figure is an average per-minute rate (sum / 5).
    let per_minute_5m =
        |total: Option<i64>| -> Option<i64> { total.map(|n| (n as f64 / 5.0).round() as i64) };

    Ok(Some(InstanceUsageRollup {
        context_used_tokens,
        context_window_tokens,
        context_pct,
        session_input_tokens: totals.session_input,
        session_output_tokens: totals.session_output,
        cache_read_tokens: totals.cache_read,
        cache_creation_tokens: totals.cache_creation,
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
        .run(move |conn| insert_usage_event(conn, &row).map_err(crate::store::StoreError::from))
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
    fn projects_folds_and_dedupes_usage_events() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let first = project_usage_event(
            &usage_record(1, 1000, Some("0.01")),
            Some("pvp_relay"),
            Some("gw/es1[1m]"),
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
        let third = project_usage_event(&third, Some("pvp_relay"), Some("gw/seed[1m]")).unwrap();
        insert_usage_event(&conn, &third).unwrap();
        let supply = aggregate_supply(&conn, "pvp_relay", "gw/seed[1m]").unwrap();
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
            mode: "snapshot".into(),
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
            context_window_tokens("claude", Some("gw/es1[1m]")),
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
}
