//! Durable wake-ups (cb-wake-engine; D-060 §11, main-agent.md §11.1, §11.3,
//! §11.4, §11.8).
//!
//! A lineage (or the owner, for any lineage) adds wake-ups with
//! `remuda wake add|list|rm`. The Hub keeps them in `hub.sqlite`, claims each
//! due occurrence once on the writer thread, and records a run:
//!
//! * **Rows (§11.1).** `wake_ups`: the lineage, a name unique in it (so an add
//!   under the same name is idempotent), the trigger (`at`, `every`, `when`),
//!   the reserved `check` (always NULL: the check is `cb-wake-check`'s), the
//!   mode, the declared `notify`, the state, the next fire time, the missed
//!   count and the creator. `wake_runs`: the wake-up, the scheduled time, the
//!   state, start and end, the check's exit code (NULL), the inbox fact and
//!   batch that carried it, a fresh run's child, and the occurrences missed
//!   before it. Idempotent DDL, so `scripts/hub-backup.sh` carries them whole.
//! * **The run state machine.** `queued` → `running` → `success`, `failed`
//!   (or `skipped`, reserved for the check). A persistent run is `queued`
//!   while its `wake-due` fact waits in the inbox, `running` from the moment
//!   its batch's forward intent is marked until the turn that batch started
//!   ends; a fresh run is `running` from the moment the Hub starts creating
//!   its child until the child's first turn ends. At boot every `running` run
//!   becomes `interrupted` and is never replayed (its fact stays consumed
//!   even if the batch is later released); the next occurrence is computed
//!   from the schedule as usual.
//! * **Missed (skipped) occurrences.** An `every` wake-up whose occurrences
//!   passed while the Hub was down fires once, for the latest one, and
//!   reports how many it skipped. An occurrence that comes due while the
//!   wake-up's previous run is still queued or running is skipped too: the
//!   schedule moves past it and it never gets a run of its own, not even
//!   once that run ends. It is added to the wake-up's missed count, which
//!   the next run reports. Nothing is merged silently.
//! * **Delivery (§11.1).** A persistent run is a `wake-due` fact for the
//!   lineage, so it waits for the idle point like any fact (§10); there is no
//!   separate busy deferral. Each batch carries at most one wake run and opens
//!   with its envelope (§11.3, `inbox_voice::wake_header`).
//! * **Result writes.** Evidence that a run ended (its turn's result, its
//!   batch refused, its chapter or child ended) is queued in
//!   `wake_outcomes` in the transaction that holds the evidence; applying it
//!   (the run row, then the `notify` declaration through
//!   `owner_notify::notify_wake`) is a separate write that is retried until it
//!   commits. Only the write is retried, never the run.
//! * **The tick.** Every [`TICK_EVERY`] scheduler ticks one writer job claims
//!   the due `at` and `every` occurrences and the `when` conditions met since
//!   the last look, with checked arithmetic: a stored interval or time outside
//!   what the routes accept is skipped and logged once, never a panic.
//! * **Fresh mode (§11.4).** The Hub creates a short-lived child under the
//!   lineage, briefed with the wake-up's text: an ordinary Agent create made
//!   with the lineage's live chapter as its caller and initiator, through the
//!   same admission (grants, scope, OA1, D-017) as any other, on the
//!   chapter's host with the one-turn `claude-print` carrier. Its facts reach
//!   the lineage through the parent edge like any child's.
//! * **Health.** `/healthz` reports the last tick and the delivery age, and
//!   answers 503 once no tick has completed for [`STALE_AFTER_TICKS`] tick
//!   periods.
//! * **Logs** carry categories, kinds and counts; never a name, a text or an
//!   id.

use crate::AppState;
use crate::agent_scope::{CallerAuthority, authenticated_caller, origin};
use crate::auth::require_origin;
use crate::config::{now_rfc3339, rfc3339};
use crate::error::{HubError, NextStep, action};
use crate::http::map_store;
use crate::owner_notify::{NoticeChapter, NoticeInput, NoticeSource, NotifyPolicy, WakeOutcome};
use crate::store::{StoreError, check_initiator, immediate_tx};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use axum::{Json, Router};
use remuda_protocol::{
    InboxFactKind, InputOrigin, WAKE_EVERY_MIN_SECS, WAKE_MAX_PER_LINEAGE, WAKE_MAX_SECS,
    WAKE_NAME_MAX_CHARS, WAKE_RUNS_LISTED, WAKE_TEXT_MAX_CHARS, WakeAddRequest, WakeAddResult,
    WakeAt, WakeCreator, WakeEvery, WakeList, WakeMode, WakeNotify, WakeRemoved, WakeRun,
    WakeRunState, WakeTrigger, WakeUp, WakeUpState,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;

/// Scheduler ticks (1 s each) between two wake evaluations.
pub(crate) const TICK_EVERY: u64 = 5;
/// Tick periods without a completed tick after which `/healthz` answers 503:
/// one minute at the 5 s period, room for a writer queue that is busy for a
/// while without hiding a scheduler that stopped.
pub(crate) const STALE_AFTER_TICKS: u64 = 12;
/// Times a queued result is applied in full before it is applied without
/// its notice. With the backoff below that is about three minutes of
/// failing writes.
const MAX_OUTCOME_ATTEMPTS: i64 = 8;
/// The first retry delay of a failed result write, in seconds; it doubles.
const OUTCOME_RETRY_FIRST_SECS: i64 = 1;
/// The longest retry delay, in seconds.
const OUTCOME_RETRY_MAX_SECS: i64 = 60;
/// Queued results one drain transaction reads.
const OUTCOME_BATCH: i64 = 32;
/// Transactions one drain runs at most; the rest wait for the next job.
const OUTCOME_BATCHES: usize = 8;
/// Fresh runs one drive claims at most.
const FRESH_PER_DRIVE: i64 = 16;
/// The carrier a fresh-mode child runs on: one turn, then the process ends.
pub(crate) const FRESH_DRIVER: &str = "claude-print";

/// The failure kinds a run can end with; each latches its own owner notice.
pub(crate) mod failure {
    /// The lineage does not receive `wake-due` facts (it muted the kind, or
    /// it receives no facts at all).
    pub(crate) const NOT_RECEIVED: &str = "not-received";
    /// The fact fell back at batch formation (muted since, or the lineage has
    /// no live chapter left).
    pub(crate) const NOT_DELIVERED: &str = "not-delivered";
    /// The batch's send was settled as refused or cancelled.
    pub(crate) const DELIVERY: &str = "delivery";
    /// The turn the batch started ended in an error.
    pub(crate) const TURN_FAILED: &str = "turn-failed";
    /// The chapter running the turn ended or was fenced first.
    pub(crate) const CHAPTER_ENDED: &str = "chapter-ended";
    /// The fresh child's create was refused.
    pub(crate) const FRESH_REFUSED: &str = "fresh-refused";
    /// The fresh child's turn ended in an error.
    pub(crate) const CHILD_FAILED: &str = "child-failed";
    /// The fresh child ended before finishing a turn.
    pub(crate) const CHILD_ENDED: &str = "child-ended";
}

// ── tables ─────────────────────────────────────────────────────────────────

/// The predicate under which a settled batch hands its rows back
/// (`inbox_deliver`'s release rule): the send provably never reached the
/// chapter.
macro_rules! released {
    () => {
        "(NEW.forwarded = 0 OR NEW.settlement_outcome = 'withheld'
          OR NEW.settlement_reason IN ('fenced', 'not-delivered', 'instance-not-found')
          OR NEW.settlement_not_dispatched = 1)"
    };
}

/// Idempotent DDL; no `user_version` rung. After `inbox_deliver::migrate`:
/// the triggers read `inbox_facts`, `commands` (with its settlement columns)
/// and `instances` (with `end_seen_at`). Triggers are rebuilt on every open,
/// so a change to their predicates reaches existing databases.
pub(crate) fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS wake_ups (
            wake_id TEXT PRIMARY KEY,
            lineage_id TEXT NOT NULL,
            name TEXT NOT NULL,
            text TEXT NOT NULL,
            trigger_kind TEXT NOT NULL,
            trigger_json TEXT NOT NULL,
            every_secs INTEGER,
            check_json TEXT,
            mode TEXT NOT NULL,
            notify TEXT NOT NULL,
            state TEXT NOT NULL,
            next_fire_at TEXT,
            missed_count INTEGER NOT NULL DEFAULT 0,
            when_cursor INTEGER NOT NULL DEFAULT 0,
            request_json TEXT NOT NULL,
            creator_origin TEXT NOT NULL,
            creator_instance_id TEXT,
            creator_device_id TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            last_fired_at TEXT,
            UNIQUE (lineage_id, name)
         );
         CREATE INDEX IF NOT EXISTS wake_ups_due
            ON wake_ups(next_fire_at) WHERE state = 'active';
         CREATE TABLE IF NOT EXISTS wake_runs (
            run_id TEXT PRIMARY KEY,
            wake_id TEXT NOT NULL,
            lineage_id TEXT NOT NULL,
            name TEXT NOT NULL,
            mode TEXT NOT NULL,
            notify TEXT NOT NULL,
            detail_json TEXT NOT NULL,
            scheduled_at TEXT NOT NULL,
            fired_at TEXT NOT NULL,
            state TEXT NOT NULL,
            started_at TEXT,
            ended_at TEXT,
            check_exit INTEGER,
            fact_id INTEGER,
            batch_id TEXT,
            target_instance_id TEXT,
            child_instance_id TEXT,
            missed_before INTEGER NOT NULL DEFAULT 0,
            failure_kind TEXT
         );
         CREATE INDEX IF NOT EXISTS wake_runs_wake ON wake_runs(wake_id, state);
         CREATE INDEX IF NOT EXISTS wake_runs_lineage ON wake_runs(lineage_id, fired_at);
         CREATE INDEX IF NOT EXISTS wake_runs_open
            ON wake_runs(state, mode) WHERE state IN ('queued', 'running');
         CREATE INDEX IF NOT EXISTS wake_runs_fact ON wake_runs(fact_id) WHERE fact_id IS NOT NULL;
         CREATE INDEX IF NOT EXISTS wake_runs_batch
            ON wake_runs(batch_id) WHERE batch_id IS NOT NULL;
         CREATE INDEX IF NOT EXISTS wake_runs_target
            ON wake_runs(target_instance_id) WHERE state = 'running';
         CREATE INDEX IF NOT EXISTS wake_runs_child
            ON wake_runs(child_instance_id) WHERE child_instance_id IS NOT NULL;
         -- The `when` scan reads a lineage's inbox rows past a cursor.
         CREATE INDEX IF NOT EXISTS wake_inbox_facts_lineage
            ON inbox_facts(subscriber_lineage_id, id);
         CREATE TABLE IF NOT EXISTS wake_outcomes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            run_id TEXT NOT NULL,
            outcome TEXT NOT NULL,
            failure_kind TEXT,
            observed_at TEXT NOT NULL,
            attempts INTEGER NOT NULL DEFAULT 0,
            next_attempt_at TEXT
         );",
    )?;
    conn.execute_batch(concat!(
        "-- The batch that carries a queued run's fact.
         DROP TRIGGER IF EXISTS wake_fact_batched;
         CREATE TRIGGER wake_fact_batched AFTER UPDATE OF batch_id ON inbox_facts
         WHEN NEW.kind = 'wake-due' AND NEW.batch_id IS NOT NULL AND OLD.batch_id IS NULL
         BEGIN
           UPDATE wake_runs SET batch_id = NEW.batch_id
            WHERE fact_id = NEW.id AND state = 'queued';
         END;
         -- A run that is no longer queued or running (interrupted at boot,
         -- or already ended) never delivers again: a release of its batch
         -- keeps its fact consumed.
         DROP TRIGGER IF EXISTS wake_fact_kept;
         CREATE TRIGGER wake_fact_kept BEFORE UPDATE OF batch_id ON inbox_facts
         WHEN OLD.kind = 'wake-due' AND OLD.batch_id IS NOT NULL AND NEW.batch_id IS NULL
          AND EXISTS (SELECT 1 FROM wake_runs
                       WHERE fact_id = OLD.id AND state NOT IN ('queued', 'running'))
         BEGIN
           SELECT RAISE(IGNORE);
         END;
         -- The batch provably never reached the chapter: the run waits again
         -- and its fact forms a new batch under a new id.
         DROP TRIGGER IF EXISTS wake_fact_released;
         CREATE TRIGGER wake_fact_released AFTER UPDATE OF batch_id ON inbox_facts
         WHEN NEW.kind = 'wake-due' AND OLD.batch_id IS NOT NULL AND NEW.batch_id IS NULL
         BEGIN
           UPDATE wake_runs
              SET state = 'queued', batch_id = NULL, started_at = NULL,
                  target_instance_id = NULL
            WHERE fact_id = NEW.id AND state IN ('queued', 'running');
         END;
         -- The fact fell back at batch formation: the run is not delivered.
         DROP TRIGGER IF EXISTS wake_fact_fallback;
         CREATE TRIGGER wake_fact_fallback AFTER UPDATE OF fallback_at ON inbox_facts
         WHEN NEW.kind = 'wake-due' AND OLD.fallback_at IS NULL AND NEW.fallback_at IS NOT NULL
         BEGIN
           INSERT INTO wake_outcomes (run_id, outcome, failure_kind, observed_at)
           SELECT run_id, 'failed', 'not-delivered', NEW.fallback_at FROM wake_runs
            WHERE fact_id = NEW.id AND state IN ('queued', 'running');
         END;
         -- The batch's forward intent is marked: the run is running on that
         -- chapter.
         DROP TRIGGER IF EXISTS wake_forwarded;
         CREATE TRIGGER wake_forwarded AFTER UPDATE OF forwarded ON commands
         WHEN NEW.forwarded = 1 AND OLD.forwarded = 0
         BEGIN
           UPDATE wake_runs
              SET state = 'running',
                  started_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                  target_instance_id = NEW.instance_id
            WHERE batch_id = NEW.id AND state = 'queued';
         END;
         -- The forward was released for a same-id retry: nothing reached the
         -- chapter, the run waits again.
         DROP TRIGGER IF EXISTS wake_unforwarded;
         CREATE TRIGGER wake_unforwarded AFTER UPDATE OF forwarded ON commands
         WHEN NEW.forwarded = 0 AND OLD.forwarded = 1 AND NEW.state = 'queued'
         BEGIN
           UPDATE wake_runs SET state = 'queued', started_at = NULL, target_instance_id = NULL
            WHERE batch_id = NEW.id AND state = 'running';
         END;
         -- The batch's send settled refused or cancelled without proof that
         -- it never reached the chapter: the run failed its delivery.
         DROP TRIGGER IF EXISTS wake_batch_refused;
         CREATE TRIGGER wake_batch_refused AFTER UPDATE OF state ON commands
         WHEN NEW.state = 'settled' AND OLD.state <> 'settled'
          AND COALESCE(NEW.settlement_outcome, 'completed') <> 'completed'
          AND NOT ",
        released!(),
        "
         BEGIN
           INSERT INTO wake_outcomes (run_id, outcome, failure_kind, observed_at)
           SELECT run_id, 'failed', 'delivery', NEW.updated_at FROM wake_runs
            WHERE batch_id = NEW.id AND state IN ('queued', 'running');
         END;
         -- The chapter (or fresh child) running a run ended or was fenced
         -- before the run's turn ended.
         DROP TRIGGER IF EXISTS wake_target_ended;
         CREATE TRIGGER wake_target_ended AFTER UPDATE OF end_seen_at ON instances
         WHEN OLD.end_seen_at IS NULL AND NEW.end_seen_at IS NOT NULL
         BEGIN
           INSERT INTO wake_outcomes (run_id, outcome, failure_kind, observed_at)
           SELECT run_id, 'failed',
                  CASE WHEN child_instance_id = NEW.id THEN 'child-ended' ELSE 'chapter-ended' END,
                  NEW.end_seen_at
             FROM wake_runs WHERE state = 'running' AND target_instance_id = NEW.id;
         END;
         DROP TRIGGER IF EXISTS wake_target_fenced;
         CREATE TRIGGER wake_target_fenced AFTER UPDATE OF fenced_at ON instances
         WHEN OLD.fenced_at IS NULL AND NEW.fenced_at IS NOT NULL
         BEGIN
           INSERT INTO wake_outcomes (run_id, outcome, failure_kind, observed_at)
           SELECT run_id, 'failed',
                  CASE WHEN child_instance_id = NEW.id THEN 'child-ended' ELSE 'chapter-ended' END,
                  NEW.fenced_at
             FROM wake_runs WHERE state = 'running' AND target_instance_id = NEW.id;
         END;
         -- A fresh run's child, recorded in the create's own transaction: only
         -- a create made by a chapter of the run's lineage while the run is
         -- running and has no child yet.
         DROP TRIGGER IF EXISTS wake_child_created;
         CREATE TRIGGER wake_child_created AFTER INSERT ON instances
         WHEN json_extract(NEW.spec_json, '$.wakeRunId') IS NOT NULL
         BEGIN
           UPDATE wake_runs SET child_instance_id = NEW.id, target_instance_id = NEW.id
            WHERE run_id = json_extract(NEW.spec_json, '$.wakeRunId')
              AND mode = 'fresh' AND state = 'running' AND child_instance_id IS NULL
              AND lineage_id = (SELECT COALESCE(p.lineage_id, p.id) FROM instances p
                                 WHERE p.id = json_extract(NEW.spec_json, '$.parentInstanceId'));
         END;"
    ))
}

// ── health ─────────────────────────────────────────────────────────────────

/// In-process state of the wake engine: what `/healthz` reports, and the
/// fresh-mode drive's serialization.
#[derive(Clone, Default)]
pub(crate) struct WakeState {
    health: Arc<Mutex<Health>>,
    /// Fresh drives are serialized; the durable claim is the real guard.
    fresh: Arc<tokio::sync::Mutex<()>>,
    /// Test seam: while set, the scheduler's ticks are skipped.
    #[cfg(any(test, feature = "test-faults"))]
    pub(crate) hold_tick: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Clone, Copy, Debug, Default)]
struct Health {
    /// Wall-clock time the last tick's writer job completed.
    last_tick: Option<OffsetDateTime>,
    /// When the oldest run still waiting for delivery fired, as of that tick.
    oldest_waiting: Option<OffsetDateTime>,
}

impl WakeState {
    pub(crate) fn note_tick(&self, oldest_waiting: Option<OffsetDateTime>) {
        let mut health = self
            .health
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        health.last_tick = Some(OffsetDateTime::now_utc());
        health.oldest_waiting = oldest_waiting;
    }

    /// Test seam: pretend the last tick completed `secs` ago.
    #[cfg(any(test, feature = "test-faults"))]
    pub(crate) fn backdate_tick(&self, secs: i64) {
        let mut health = self
            .health
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        health.last_tick =
            Some(OffsetDateTime::now_utc() - time::Duration::seconds(secs.clamp(0, WAKE_MAX_SECS)));
    }
}

/// The tick and delivery fields `/healthz` reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HealthReport {
    pub(crate) last_tick_at: Option<String>,
    pub(crate) tick_age_secs: u64,
    pub(crate) delivery_age_secs: Option<u64>,
    pub(crate) stale: bool,
}

/// The tick period in seconds.
pub(crate) fn tick_period_secs() -> u64 {
    TICK_EVERY * crate::gatequeue::SCHEDULE_TICK_MILLIS / 1000
}

/// Seconds without a completed tick after which the tick is stale.
pub(crate) fn stale_after_secs() -> u64 {
    tick_period_secs() * STALE_AFTER_TICKS
}

/// The health of the tick as of `now`. Before the first tick completes the
/// age counts from `started`, so a Hub whose scheduler never ran turns stale
/// too.
pub(crate) fn health(state: &AppState, now: OffsetDateTime) -> HealthReport {
    let health = *state
        .wake
        .health
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let started = state.store.opened_at();
    let since = health.last_tick.unwrap_or(started);
    let age = |at: OffsetDateTime| u64::try_from((now - at).whole_seconds()).unwrap_or(0);
    let tick_age_secs = age(since);
    HealthReport {
        last_tick_at: health.last_tick.map(rfc3339),
        tick_age_secs,
        delivery_age_secs: health.oldest_waiting.map(age),
        stale: tick_age_secs > stale_after_secs(),
    }
}

// ── validation ─────────────────────────────────────────────────────────────

/// A refusal of the wake routes' own (the rest are [`HubError`]s).
#[derive(Debug)]
pub(crate) enum WakeError {
    /// The body is unusable.
    BadRequest(String),
    /// The name or text looks like a credential; the text is not echoed.
    SecretRejected {
        field: &'static str,
        rule: &'static str,
    },
    /// A field names something that is not built yet.
    Unsupported {
        reason: &'static str,
        message: &'static str,
    },
    /// The lineage cannot take it.
    Conflict {
        reason: &'static str,
        message: String,
        hint: &'static str,
    },
    /// Any other refusal.
    Hub(HubError),
}

impl From<HubError> for WakeError {
    fn from(error: HubError) -> Self {
        Self::Hub(error)
    }
}

impl IntoResponse for WakeError {
    fn into_response(self) -> Response {
        match self {
            Self::Hub(error) => error.into_response(),
            Self::BadRequest(message) => HubError::BadRequest(message)
                .with_next(
                    "invalid-wake-up",
                    NextStep::new(
                        action::FIX_REQUEST,
                        format!(
                            "send a name of at most {WAKE_NAME_MAX_CHARS} characters from \
                             [A-Za-z0-9._:-] not starting with wak_, a text of at most \
                             {WAKE_TEXT_MAX_CHARS} \
                             characters, one trigger (at a time or afterSecs within a year; \
                             every {WAKE_EVERY_MIN_SECS}..={WAKE_MAX_SECS} seconds; when a \
                             fact kind) and notify never, failure or always"
                        ),
                    ),
                )
                .into_response(),
            Self::SecretRejected { field, rule } => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "error": format!(
                        "the wake-up was not stored: its {field} looks like a credential \
                         (shape {rule:?}); the text is not echoed"
                    ),
                    "code": "WAKE_SECRET_REJECTED",
                    "reason": "secret-rejected",
                    "field": field,
                    "rule": rule,
                    "next": NextStep::new(
                        action::FIX_REQUEST,
                        "remove the credential and add the wake-up again; say where the \
                         secret is kept, never the secret itself",
                    )
                    .json(),
                })),
            )
                .into_response(),
            Self::Unsupported { reason, message } => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "error": message,
                    "code": "WAKE_UNSUPPORTED",
                    "reason": reason,
                    "next": NextStep::new(
                        action::FIX_REQUEST,
                        "remove the field and add the wake-up again; nothing was stored",
                    )
                    .json(),
                })),
            )
                .into_response(),
            Self::Conflict {
                reason,
                message,
                hint,
            } => HubError::Conflict(message)
                .with_next(reason, NextStep::new(action::FIX_REQUEST, hint))
                .into_response(),
        }
    }
}

/// A checked add, ready for the writer.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CheckedAdd {
    /// `None`: the Hub names it.
    pub(crate) name: Option<String>,
    pub(crate) text: String,
    /// As stored: an `at` carries its resolved time.
    pub(crate) trigger: WakeTrigger,
    /// The first occurrence (`at`, `every`); `None` for `when`.
    pub(crate) first_fire: Option<String>,
    pub(crate) every_secs: Option<i64>,
    pub(crate) mode: WakeMode,
    pub(crate) notify: WakeNotify,
    /// The request as sent, canonical: what a repeat must match.
    pub(crate) request_json: String,
}

/// A wake-up name: a path segment (`rm` takes it in the URL) that cannot
/// read as a wake-up id (`rm` takes either).
fn wake_name(raw: &str) -> bool {
    !raw.starts_with("wak_")
        && raw.chars().count() <= WAKE_NAME_MAX_CHARS
        && !raw.is_empty()
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
        && raw != "."
        && raw != ".."
}

fn plain_token(raw: &str, max: usize) -> bool {
    !raw.is_empty()
        && raw.chars().count() <= max
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '-'))
}

/// `raw` as a time no further than a year after `now`.
fn bounded_time(field: &str, raw: &str, now: OffsetDateTime) -> Result<OffsetDateTime, WakeError> {
    let at = crate::inbox::parse_at(raw)
        .ok_or_else(|| WakeError::BadRequest(format!("{field} must be an RFC3339 time")))?;
    let latest = now
        .checked_add(time::Duration::seconds(WAKE_MAX_SECS))
        .ok_or_else(|| WakeError::BadRequest(format!("{field} is out of range")))?;
    if at > latest {
        return Err(WakeError::BadRequest(format!(
            "{field} must be within {WAKE_MAX_SECS} seconds of now"
        )));
    }
    Ok(at)
}

/// Bound the fields, run the secret lint over the name and the text (first,
/// so a credential is refused as one whatever else is wrong), refuse what is
/// not built yet, and resolve the trigger against `now`.
pub(crate) fn check_add(
    request: &WakeAddRequest,
    now: OffsetDateTime,
) -> Result<CheckedAdd, WakeError> {
    for (field, text) in [
        ("name", request.name.as_deref()),
        ("text", Some(&request.text)),
    ] {
        if let Some(rule) = text.and_then(crate::secret_lint::first_secret) {
            return Err(WakeError::SecretRejected { field, rule });
        }
    }
    if request.check.as_ref().is_some_and(|check| !check.is_null()) {
        return Err(WakeError::Unsupported {
            reason: "check-unsupported",
            message: "a wake-up check is not supported yet (main-agent.md §11.2); nothing \
                      was stored",
        });
    }
    let name = match request.name.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(name) if wake_name(name) => Some(name.to_owned()),
        Some(_) => {
            return Err(WakeError::BadRequest(format!(
                "name must be at most {WAKE_NAME_MAX_CHARS} characters from [A-Za-z0-9._:-], \
                 not starting with wak_"
            )));
        }
    };
    let text = request.text.trim();
    if text.is_empty() {
        return Err(WakeError::BadRequest("text must not be empty".into()));
    }
    let chars = text.chars().count();
    if chars > WAKE_TEXT_MAX_CHARS {
        return Err(WakeError::BadRequest(format!(
            "text is {chars} characters; at most {WAKE_TEXT_MAX_CHARS}"
        )));
    }
    let (trigger, first_fire, every_secs) = match &request.trigger {
        WakeTrigger::At(at) => {
            let fire = match (at.at.as_deref(), at.after_secs) {
                (Some(raw), None) => bounded_time("at", raw, now)?,
                (None, Some(after)) => {
                    if !(0..=WAKE_MAX_SECS).contains(&after) {
                        return Err(WakeError::BadRequest(format!(
                            "afterSecs must be between 0 and {WAKE_MAX_SECS}"
                        )));
                    }
                    now.checked_add(time::Duration::seconds(after))
                        .ok_or_else(|| WakeError::BadRequest("afterSecs is out of range".into()))?
                }
                _ => {
                    return Err(WakeError::BadRequest(
                        "an at trigger takes exactly one of at and afterSecs".into(),
                    ));
                }
            };
            let fire = rfc3339(fire);
            (
                WakeTrigger::At(WakeAt {
                    at: Some(fire.clone()),
                    after_secs: None,
                }),
                Some(fire),
                None,
            )
        }
        WakeTrigger::Every(every) => {
            if !(WAKE_EVERY_MIN_SECS..=WAKE_MAX_SECS).contains(&every.every_secs) {
                return Err(WakeError::BadRequest(format!(
                    "everySecs must be between {WAKE_EVERY_MIN_SECS} and {WAKE_MAX_SECS}"
                )));
            }
            let first = match every.start_at.as_deref() {
                Some(raw) => bounded_time("startAt", raw, now)?,
                None => now
                    .checked_add(time::Duration::seconds(every.every_secs))
                    .ok_or_else(|| WakeError::BadRequest("everySecs is out of range".into()))?,
            };
            (
                WakeTrigger::Every(WakeEvery {
                    every_secs: every.every_secs,
                    start_at: every.start_at.as_deref().map(|_| rfc3339(first)),
                }),
                Some(rfc3339(first)),
                Some(every.every_secs),
            )
        }
        WakeTrigger::When(when) => {
            match when.fact_kind {
                InboxFactKind::WakeDue => {
                    return Err(WakeError::BadRequest(
                        "a wake-up cannot fire on wake-due facts".into(),
                    ));
                }
                InboxFactKind::ContextThreshold => {
                    return Err(WakeError::Unsupported {
                        reason: "condition-unsupported",
                        message: "a context-threshold condition is not supported yet: nothing \
                                  records context-percentage crossings; nothing was stored",
                    });
                }
                _ => {}
            }
            if let Some(subject) = &when.subject_instance_id
                && !plain_token(subject, 96)
            {
                return Err(WakeError::BadRequest(
                    "subjectInstanceId must be an instance id".into(),
                ));
            }
            (WakeTrigger::When(when.clone()), None, None)
        }
    };
    let mode = request.mode.unwrap_or(WakeMode::Persistent);
    let request_json = json!({
        "text": text,
        "trigger": request.trigger,
        "mode": mode,
        "notify": request.notify,
    })
    .to_string();
    Ok(CheckedAdd {
        name,
        text: text.to_owned(),
        trigger,
        first_fire,
        every_secs,
        mode,
        notify: request.notify,
        request_json,
    })
}

// ── rows ───────────────────────────────────────────────────────────────────

const WAKE_COLUMNS: &str = "wake_id, lineage_id, name, text, trigger_json, check_json, mode, \
     notify, state, next_fire_at, missed_count, created_at, updated_at, last_fired_at, \
     creator_origin, creator_instance_id";

fn wake_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WakeUp> {
    let trigger: String = row.get(4)?;
    let check: Option<String> = row.get(5)?;
    let mode: String = row.get(6)?;
    let notify: String = row.get(7)?;
    let state: String = row.get(8)?;
    let creator: String = row.get(14)?;
    Ok(WakeUp {
        wake_id: row.get(0)?,
        lineage_id: row.get(1)?,
        name: row.get(2)?,
        text: row.get(3)?,
        // A row this build cannot read keeps its other fields; the trigger
        // reads as an `at` with no time rather than failing the list.
        trigger: serde_json::from_str(&trigger)
            .unwrap_or_else(|_| WakeTrigger::At(WakeAt::default())),
        check: check.and_then(|raw| serde_json::from_str(&raw).ok()),
        mode: WakeMode::parse(&mode).unwrap_or(WakeMode::Persistent),
        notify: WakeNotify::parse(&notify).unwrap_or(WakeNotify::Never),
        state: WakeUpState::parse(&state).unwrap_or(WakeUpState::Done),
        next_fire_at: row.get(9)?,
        missed_count: row.get(10)?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
        last_fired_at: row.get(13)?,
        creator: WakeCreator {
            origin: if creator == "human" {
                InputOrigin::Human
            } else {
                InputOrigin::Agent
            },
            instance_id: row.get(15)?,
        },
    })
}

const RUN_COLUMNS: &str = "run_id, wake_id, lineage_id, name, mode, scheduled_at, fired_at, \
     state, started_at, ended_at, check_exit, fact_id, batch_id, child_instance_id, \
     missed_before, failure_kind";

fn run_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WakeRun> {
    let mode: String = row.get(4)?;
    let state: String = row.get(7)?;
    Ok(WakeRun {
        run_id: row.get(0)?,
        wake_id: row.get(1)?,
        lineage_id: row.get(2)?,
        name: row.get(3)?,
        mode: WakeMode::parse(&mode).unwrap_or(WakeMode::Persistent),
        scheduled_at: row.get(5)?,
        fired_at: row.get(6)?,
        state: WakeRunState::parse(&state).unwrap_or(WakeRunState::Failed),
        started_at: row.get(8)?,
        ended_at: row.get(9)?,
        check_exit: row.get(10)?,
        fact_id: row.get(11)?,
        batch_id: row.get(12)?,
        child_instance_id: row.get(13)?,
        missed_before: row.get(14)?,
        failure_kind: row.get(15)?,
    })
}

fn load_wake(conn: &Connection, wake_id: &str) -> Result<Option<WakeUp>, StoreError> {
    Ok(conn
        .query_row(
            &format!("SELECT {WAKE_COLUMNS} FROM wake_ups WHERE wake_id = ?1"),
            params![wake_id],
            wake_row,
        )
        .optional()?)
}

/// Whether `lineage_id` names a lineage: a continuity lineage, or an
/// instance that is its own lineage.
fn lineage_exists(conn: &Connection, lineage_id: &str) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM lineages WHERE lineage_id = ?1)
             OR EXISTS (SELECT 1 FROM instances WHERE id = ?1)",
        params![lineage_id],
        |row| row.get(0),
    )?)
}

/// Who an add or a removal is from.
#[derive(Clone, Debug)]
pub(crate) struct WakeCaller {
    pub(crate) origin: InputOrigin,
    pub(crate) lineage_id: String,
    pub(crate) authority: CallerAuthority,
    /// The credential the call authenticated as, for the audit row.
    pub(crate) device_id: String,
}

/// What an add did, or why the lineage cannot take it.
#[derive(Debug)]
pub(crate) enum Added {
    Created(WakeUp),
    Existing(WakeUp),
    NameTaken,
    Limit,
    NotReceived,
    NoLineage,
}

/// One add, in one writer transaction.
pub(crate) fn add(
    conn: &mut Connection,
    caller: &WakeCaller,
    checked: &CheckedAdd,
) -> Result<Added, StoreError> {
    let tx = immediate_tx(conn)?;
    // D-057 §7.3: a fenced, superseded or paused chapter writes nothing.
    let (initiator, device_id) = caller.authority.as_check();
    check_initiator(&tx, initiator, device_id)?;
    let lineage = caller.lineage_id.as_str();
    if !lineage_exists(&tx, lineage)? {
        return Ok(Added::NoLineage);
    }
    if let Some(name) = &checked.name {
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT wake_id, request_json FROM wake_ups WHERE lineage_id = ?1 AND name = ?2",
                params![lineage, name],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((wake_id, request_json)) = existing {
            return Ok(if request_json == checked.request_json {
                load_wake(&tx, &wake_id)?.map_or(Added::NameTaken, Added::Existing)
            } else {
                Added::NameTaken
            });
        }
    }
    // §11.1: a wake-up is delivered through the inbox (and a fresh child's
    // facts come back through it), so the lineage must receive facts.
    if !crate::inbox::receives_any(&tx, lineage)? {
        return Ok(Added::NotReceived);
    }
    let held: i64 = tx.query_row(
        "SELECT COUNT(*) FROM wake_ups WHERE lineage_id = ?1",
        params![lineage],
        |row| row.get(0),
    )?;
    if usize::try_from(held).unwrap_or(usize::MAX) >= WAKE_MAX_PER_LINEAGE {
        return Ok(Added::Limit);
    }
    let wake_id = format!("wak_{}", uuid::Uuid::now_v7());
    let name = checked.name.clone().unwrap_or_else(|| {
        let id = wake_id.trim_start_matches("wak_").replace('-', "");
        format!("wake-{}", &id[id.len().saturating_sub(12)..])
    });
    // A `when` counts facts recorded after the add only.
    let cursor: i64 = match checked.trigger {
        WakeTrigger::When(_) => tx.query_row(
            "SELECT COALESCE(MAX(id), 0) FROM inbox_facts WHERE subscriber_lineage_id = ?1",
            params![lineage],
            |row| row.get(0),
        )?,
        _ => 0,
    };
    let now = now_rfc3339();
    let creator_origin = if caller.origin == InputOrigin::Human {
        "human"
    } else {
        "agent"
    };
    let creator_instance = caller
        .authority
        .initiator
        .as_ref()
        .map(|initiator| initiator.instance_id.clone());
    tx.execute(
        "INSERT INTO wake_ups
            (wake_id, lineage_id, name, text, trigger_kind, trigger_json, every_secs, check_json,
             mode, notify, state, next_fire_at, missed_count, when_cursor, request_json,
             creator_origin, creator_instance_id, creator_device_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8, ?9, 'active', ?10, 0, ?11, ?12,
                 ?13, ?14, ?15, ?16, ?16)",
        params![
            wake_id,
            lineage,
            name,
            checked.text,
            checked.trigger.kind(),
            serde_json::to_string(&checked.trigger)?,
            checked.every_secs,
            checked.mode.as_str(),
            checked.notify.as_str(),
            checked.first_fire,
            cursor,
            checked.request_json,
            creator_origin,
            creator_instance,
            caller.device_id,
            now,
        ],
    )?;
    // The audit row names who added what kind of wake-up; the name and the
    // text stay in the row.
    tx.execute(
        "INSERT INTO audit_log (device_id, action, subject, detail_json, created_at, initiator_json)
         VALUES (?1, 'wake.add', ?2, ?3, ?4, ?5)",
        params![
            caller.device_id,
            wake_id,
            json!({
                "lineageId": lineage,
                "trigger": checked.trigger.kind(),
                "mode": checked.mode.as_str(),
                "notify": checked.notify.as_str(),
                "origin": creator_origin,
            })
            .to_string(),
            now,
            caller
                .authority
                .initiator
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
        ],
    )?;
    let stored = load_wake(&tx, &wake_id)?
        .ok_or_else(|| StoreError::Id("wake-up vanished in its own transaction".into()))?;
    tx.commit()?;
    Ok(Added::Created(stored))
}

/// The lineage's wake-ups, oldest first, and its latest runs.
pub(crate) fn list(conn: &Connection, lineage_id: &str) -> Result<WakeList, StoreError> {
    let items = conn
        .prepare_cached(&format!(
            "SELECT {WAKE_COLUMNS} FROM wake_ups WHERE lineage_id = ?1
              ORDER BY created_at, wake_id"
        ))?
        .query_map(params![lineage_id], wake_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let runs = conn
        .prepare_cached(&format!(
            "SELECT {RUN_COLUMNS} FROM wake_runs WHERE lineage_id = ?1
              ORDER BY fired_at DESC, run_id DESC LIMIT ?2"
        ))?
        .query_map(
            params![lineage_id, i64::try_from(WAKE_RUNS_LISTED).unwrap_or(50)],
            run_row,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(WakeList {
        lineage_id: lineage_id.to_owned(),
        items,
        runs,
    })
}

/// Remove one wake-up by id or name. Its runs stay as history; a run already
/// queued or running ends as it would have.
pub(crate) fn remove(
    conn: &mut Connection,
    caller: &WakeCaller,
    wake: &str,
) -> Result<Option<WakeRemoved>, StoreError> {
    let tx = immediate_tx(conn)?;
    let (initiator, device_id) = caller.authority.as_check();
    check_initiator(&tx, initiator, device_id)?;
    let found: Option<(String, String)> = tx
        .query_row(
            "SELECT wake_id, name FROM wake_ups
              WHERE lineage_id = ?1 AND (wake_id = ?2 OR name = ?2)",
            params![caller.lineage_id, wake],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((wake_id, name)) = found else {
        return Ok(None);
    };
    tx.execute("DELETE FROM wake_ups WHERE wake_id = ?1", params![wake_id])?;
    tx.execute(
        "INSERT INTO audit_log (device_id, action, subject, detail_json, created_at, initiator_json)
         VALUES (?1, 'wake.remove', ?2, ?3, ?4, ?5)",
        params![
            caller.device_id,
            wake_id,
            json!({ "lineageId": caller.lineage_id }).to_string(),
            now_rfc3339(),
            caller
                .authority
                .initiator
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
        ],
    )?;
    tx.commit()?;
    Ok(Some(WakeRemoved { wake_id, name }))
}

// ── HTTP ───────────────────────────────────────────────────────────────────

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/lineages/{id}/wake-ups",
            get(list_route).post(add_route),
        )
        .route("/v1/lineages/{id}/wake-ups/{wake}", delete(remove_route))
}

/// 403 for a caller that may not manage these wake-ups.
fn not_the_lineage() -> HubError {
    HubError::Forbidden.with_next(
        "not-own-lineage",
        NextStep::new(
            action::FIX_REQUEST,
            "an agent manages only its own lineage's wake-ups (the owner manages any); do not \
             retry for another lineage — name your own, or ask the owner",
        ),
    )
}

/// Who may use the wake routes of `id`, resolved to the lineage: the
/// lineage itself (an Agent, by its canonical lineage id) or the owner (a
/// Human device, naming the lineage or any chapter of it). A Bot credential
/// is refused on how it authenticated, whatever instance it names.
async fn wake_caller(
    state: &AppState,
    headers: &HeaderMap,
    id: String,
) -> Result<WakeCaller, HubError> {
    require_origin(headers, &state.config)?;
    let (authenticated, device) = authenticated_caller(state, headers).await?;
    if origin(&authenticated) == InputOrigin::Bot {
        return Err(not_the_lineage());
    }
    match origin(&device) {
        InputOrigin::Agent => {
            let bound = device.instance_id.clone().ok_or_else(not_the_lineage)?;
            let own = state
                .store
                .lineage_id_for_instance(bound)
                .await
                .map_err(map_store)?
                .ok_or_else(HubError::stale_caller)?;
            if own != id {
                return Err(not_the_lineage());
            }
            Ok(WakeCaller {
                origin: InputOrigin::Agent,
                lineage_id: own,
                authority: CallerAuthority::for_device(state, &device).await?,
                device_id: authenticated.id,
            })
        }
        InputOrigin::Human => {
            let lineage = state
                .store
                .lineage_id_for_instance(id)
                .await
                .map_err(map_store)?
                .ok_or(HubError::NotFound)?;
            Ok(WakeCaller {
                origin: InputOrigin::Human,
                lineage_id: lineage,
                authority: CallerAuthority::internal(),
                device_id: authenticated.id,
            })
        }
        _ => Err(not_the_lineage()),
    }
}

async fn add_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(request): Json<WakeAddRequest>,
) -> Result<Response, WakeError> {
    let caller = wake_caller(&state, &headers, id).await?;
    let checked = check_add(&request, OffsetDateTime::now_utc())?;
    let added = state
        .store
        .run_named("wake_add", {
            let caller = caller.clone();
            move |conn| add(conn, &caller, &checked)
        })
        .await
        .map_err(map_store)?;
    let (status, wake_up, created) = match added {
        Added::Created(wake) => (StatusCode::CREATED, wake, true),
        Added::Existing(wake) => (StatusCode::OK, wake, false),
        Added::NameTaken => {
            return Err(WakeError::Conflict {
                reason: "wake-name-taken",
                message: "the lineage already has a different wake-up under this name".into(),
                hint: "remove it first (remuda wake rm <name>) or add under another name",
            });
        }
        Added::Limit => {
            return Err(WakeError::Conflict {
                reason: "wake-limit",
                message: format!(
                    "the lineage already holds {WAKE_MAX_PER_LINEAGE} wake-ups, the most it may"
                ),
                hint: "remove a wake-up it no longer needs, then add again",
            });
        }
        Added::NotReceived => {
            return Err(WakeError::Conflict {
                reason: "wake-not-received",
                message: "the lineage receives no inbox facts, so no wake-up can reach it".into(),
                hint: "only a continuity lineage, an agent-created coordinator or a lineage \
                       the owner opted in receives facts; ask the owner to opt it in",
            });
        }
        Added::NoLineage => return Err(HubError::NotFound.into()),
    };
    tracing::info!(
        category = "wake",
        outcome = if created { "added" } else { "add-repeated" },
        trigger = wake_up.trigger.kind(),
        mode = wake_up.mode.as_str(),
        notify = wake_up.notify.as_str(),
        caller = if caller.origin == InputOrigin::Human {
            "human"
        } else {
            "agent"
        },
    );
    Ok((status, Json(WakeAddResult { wake_up, created })).into_response())
}

async fn list_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<WakeList>, HubError> {
    let caller = wake_caller(&state, &headers, id).await?;
    let lineage = caller.lineage_id;
    Ok(Json(
        state
            .store
            .read("wake_list", move |conn| list(conn, &lineage))
            .await
            .map_err(map_store)?,
    ))
}

async fn remove_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, wake)): Path<(String, String)>,
) -> Result<Json<WakeRemoved>, HubError> {
    let caller = wake_caller(&state, &headers, id).await?;
    let removed = state
        .store
        .run_named("wake_remove", move |conn| remove(conn, &caller, &wake))
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    tracing::info!(category = "wake", outcome = "removed");
    Ok(Json(removed))
}

// ── the tick ───────────────────────────────────────────────────────────────

/// What a due `at` or `every` wake-up fires now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Due {
    /// Occurrences due, the fired one included.
    pub(crate) count: i64,
    /// The latest one due: the occurrence the run stands for.
    pub(crate) latest: OffsetDateTime,
    /// The occurrence after it; `None` once an `at` has fired.
    pub(crate) next: Option<OffsetDateTime>,
}

/// The occurrences of a wake-up whose next one is `next`, as of `now`, in
/// checked arithmetic. `Ok(None)` when none is due; `Err` names the category
/// of a stored value no route would accept (the tick skips that wake-up and
/// logs the category once).
pub(crate) fn due_occurrences(
    kind: &str,
    next: OffsetDateTime,
    every_secs: Option<i64>,
    now: OffsetDateTime,
) -> Result<Option<Due>, &'static str> {
    if next > now {
        return Ok(None);
    }
    match kind {
        "at" => Ok(Some(Due {
            count: 1,
            latest: next,
            next: None,
        })),
        "every" => {
            let every = every_secs
                .filter(|secs| (1..=WAKE_MAX_SECS).contains(secs))
                .ok_or("wake-interval-out-of-range")?;
            let elapsed = (now - next).whole_seconds().max(0);
            let count = (elapsed / every)
                .checked_add(1)
                .ok_or("wake-schedule-overflow")?;
            let latest = (count - 1)
                .checked_mul(every)
                .and_then(|secs| next.checked_add(time::Duration::seconds(secs)))
                .ok_or("wake-schedule-overflow")?;
            let following = latest
                .checked_add(time::Duration::seconds(every))
                .ok_or("wake-schedule-overflow")?;
            Ok(Some(Due {
                count,
                latest,
                next: Some(following),
            }))
        }
        _ => Err("wake-trigger-unknown"),
    }
}

/// What one tick did.
#[derive(Debug, Default, Clone)]
pub(crate) struct TickOutcome {
    /// Runs it recorded.
    pub(crate) fired: usize,
    /// Occurrences it added to a busy wake-up's missed count.
    pub(crate) missed: i64,
    /// A fresh run waits for its child.
    pub(crate) fresh_due: bool,
    /// When the oldest run still waiting for delivery fired.
    pub(crate) oldest_waiting: Option<OffsetDateTime>,
}

/// One wake-up as the tick reads it.
#[derive(Debug, Clone)]
struct Wake {
    wake_id: String,
    lineage_id: String,
    name: String,
    text: String,
    trigger_kind: String,
    trigger_json: String,
    every_secs: Option<i64>,
    mode: String,
    notify: String,
    next_fire_at: Option<String>,
    missed_count: i64,
    when_cursor: i64,
    creator_origin: String,
}

const TICK_COLUMNS: &str = "wake_id, lineage_id, name, text, trigger_kind, trigger_json, \
     every_secs, mode, notify, next_fire_at, missed_count, when_cursor, creator_origin";

fn tick_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Wake> {
    Ok(Wake {
        wake_id: row.get(0)?,
        lineage_id: row.get(1)?,
        name: row.get(2)?,
        text: row.get(3)?,
        trigger_kind: row.get(4)?,
        trigger_json: row.get(5)?,
        every_secs: row.get(6)?,
        mode: row.get(7)?,
        notify: row.get(8)?,
        next_fire_at: row.get(9)?,
        missed_count: row.get(10)?,
        when_cursor: row.get(11)?,
        creator_origin: row.get(12)?,
    })
}

/// Why a wake-up fired, beyond its schedule.
#[derive(Debug, Clone)]
struct Fire {
    scheduled_at: String,
    /// Occurrences due now, the fired one included.
    due: i64,
    /// The fact that met a `when` condition.
    cause: Option<(i64, InboxFactKind)>,
}

/// Claim one occurrence of `wake`: a run, unless an earlier run of it is
/// still queued or running. Then the occurrences due now are skipped: the
/// caller has already moved the schedule past them, so none of them ever
/// gets a run of its own, not even once the open run ends. They are counted
/// in the wake-up's missed count, which the next run reports as skipped.
fn fire(
    conn: &Connection,
    wake: &Wake,
    occurrence: Fire,
    now: &str,
    outcome: &mut TickOutcome,
) -> Result<(), StoreError> {
    let busy: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM wake_runs
                         WHERE wake_id = ?1 AND state IN ('queued', 'running'))",
        params![wake.wake_id],
        |row| row.get(0),
    )?;
    if busy {
        // Every occurrence due now is skipped (none of them fired), so all
        // `due` of them count; the fire path below counts `due - 1` because
        // it fires the latest one.
        conn.execute(
            "UPDATE wake_ups SET missed_count = ?2, updated_at = ?3 WHERE wake_id = ?1",
            params![
                wake.wake_id,
                wake.missed_count.saturating_add(occurrence.due),
                now
            ],
        )?;
        outcome.missed = outcome.missed.saturating_add(occurrence.due);
        tracing::info!(
            category = "wake",
            outcome = "missed-while-busy",
            occurrences = occurrence.due
        );
        return Ok(());
    }
    let missed = wake
        .missed_count
        .saturating_add(occurrence.due.saturating_sub(1))
        .max(0);
    let run_id = format!("wrn_{}", uuid::Uuid::now_v7());
    let mut detail = json!({
        "wakeId": wake.wake_id,
        "runId": run_id,
        "name": wake.name,
        "text": wake.text,
        "trigger": wake.trigger_kind,
        "mode": wake.mode,
        "scheduledAt": occurrence.scheduled_at,
        "firedAt": now,
        "missed": missed,
        "creator": wake.creator_origin,
    });
    if let Some(every) = wake.every_secs.filter(|_| wake.trigger_kind == "every") {
        detail["everySecs"] = json!(every);
    }
    if let Some((fact_id, kind)) = occurrence.cause {
        detail["cause"] = json!({ "factId": fact_id, "factKind": kind.as_str() });
    }
    conn.execute(
        "INSERT INTO wake_runs
            (run_id, wake_id, lineage_id, name, mode, notify, detail_json, scheduled_at,
             fired_at, state, missed_before)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'queued', ?10)",
        params![
            run_id,
            wake.wake_id,
            wake.lineage_id,
            wake.name,
            wake.mode,
            wake.notify,
            detail.to_string(),
            occurrence.scheduled_at,
            now,
            missed,
        ],
    )?;
    conn.execute(
        "UPDATE wake_ups SET missed_count = 0, last_fired_at = ?2, updated_at = ?2
          WHERE wake_id = ?1",
        params![wake.wake_id, now],
    )?;
    outcome.fired += 1;
    if wake.mode == WakeMode::Fresh.as_str() {
        outcome.fresh_due = true;
    } else {
        // §11.1: delivered through the inbox, so it waits for the idle point
        // like any fact. Each run is its own fact (its id is the key and the
        // cap subject), so the occurrence cap never folds two runs together.
        let recorded = crate::inbox::record(
            conn,
            &wake.lineage_id,
            crate::inbox::Fact {
                kind: InboxFactKind::WakeDue,
                subject_instance_id: None,
                key_subject: run_id.clone(),
                cap_subject: run_id.clone(),
                run: 0,
                run_in_key: false,
                entered_at: now.to_owned(),
                project_id: None,
                payload: detail,
            },
        )?;
        match recorded {
            crate::inbox::Outcome::Recorded(fact_id) => {
                conn.execute(
                    "UPDATE wake_runs SET fact_id = ?2 WHERE run_id = ?1",
                    params![run_id, fact_id],
                )?;
            }
            // Muted since the add, or the lineage no longer receives facts:
            // the run cannot be delivered.
            _ => queue_outcome(conn, &run_id, false, Some(failure::NOT_RECEIVED), now)?,
        }
    }
    tracing::info!(
        category = "wake",
        outcome = "fired",
        trigger = wake.trigger_kind.as_str(),
        mode = wake.mode.as_str(),
        missed
    );
    Ok(())
}

/// Run `work` for one wake-up inside a savepoint: a wake-up whose row this
/// build cannot use rolls back alone and never stops the others.
fn isolated(
    conn: &Connection,
    work: impl FnOnce() -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    conn.execute_batch("SAVEPOINT wake_one")?;
    match work() {
        Ok(()) => {
            conn.execute_batch("RELEASE wake_one")?;
            Ok(())
        }
        Err(err) => {
            conn.execute_batch("ROLLBACK TO wake_one; RELEASE wake_one")?;
            tracing::error!(
                category = "wake",
                code = crate::inbox::error_code(&err),
                "a wake-up could not be evaluated; the others were"
            );
            Ok(())
        }
    }
}

/// The due `at` and `every` occurrences.
fn due_scheduled(
    conn: &Connection,
    now: OffsetDateTime,
    now_s: &str,
    outcome: &mut TickOutcome,
) -> Result<(), StoreError> {
    let due: Vec<Wake> = conn
        .prepare_cached(&format!(
            "SELECT {TICK_COLUMNS} FROM wake_ups
              WHERE state = 'active' AND trigger_kind IN ('at', 'every')
                AND next_fire_at IS NOT NULL AND next_fire_at <= ?1
              ORDER BY next_fire_at, wake_id"
        ))?
        .query_map(params![now_s], tick_row)?
        .collect::<rusqlite::Result<_>>()?;
    for wake in due {
        isolated(conn, || {
            if !lineage_exists(conn, &wake.lineage_id)? {
                conn.execute(
                    "UPDATE wake_ups SET state = 'done', next_fire_at = NULL, updated_at = ?2
                      WHERE wake_id = ?1",
                    params![wake.wake_id, now_s],
                )?;
                tracing::info!(category = "wake", outcome = "lineage-gone");
                return Ok(());
            }
            let Some(next) = wake
                .next_fire_at
                .as_deref()
                .and_then(crate::inbox::parse_at)
            else {
                crate::inbox::report_once("wake-time-unreadable");
                return Ok(());
            };
            let due = match due_occurrences(&wake.trigger_kind, next, wake.every_secs, now) {
                Ok(Some(due)) => due,
                Ok(None) => return Ok(()),
                Err(code) => {
                    crate::inbox::report_once(code);
                    return Ok(());
                }
            };
            conn.execute(
                "UPDATE wake_ups SET next_fire_at = ?2, state = ?3, updated_at = ?4
                  WHERE wake_id = ?1",
                params![
                    wake.wake_id,
                    due.next.map(rfc3339),
                    if due.next.is_some() { "active" } else { "done" },
                    now_s
                ],
            )?;
            fire(
                conn,
                &wake,
                Fire {
                    scheduled_at: rfc3339(due.latest),
                    due: due.count,
                    cause: None,
                },
                now_s,
                outcome,
            )
        })?;
    }
    Ok(())
}

/// The `when` conditions met since each one's cursor: one run for the facts
/// that met it, the latest standing for them. Facts about a wake run's own
/// fresh child never count, so a wake-up cannot keep re-firing itself.
fn due_conditions(
    conn: &Connection,
    now_s: &str,
    outcome: &mut TickOutcome,
) -> Result<(), StoreError> {
    let candidates: Vec<Wake> = conn
        .prepare_cached(&format!(
            "SELECT {TICK_COLUMNS} FROM wake_ups w
              WHERE state = 'active' AND trigger_kind = 'when'
                AND EXISTS (SELECT 1 FROM inbox_facts f
                             WHERE f.subscriber_lineage_id = w.lineage_id
                               AND f.id > w.when_cursor)
              ORDER BY created_at, wake_id"
        ))?
        .query_map([], tick_row)?
        .collect::<rusqlite::Result<_>>()?;
    for wake in candidates {
        isolated(conn, || {
            let head: i64 = conn.query_row(
                "SELECT COALESCE(MAX(id), ?2) FROM inbox_facts WHERE subscriber_lineage_id = ?1",
                params![wake.lineage_id, wake.when_cursor],
                |row| row.get(0),
            )?;
            // The cursor moves over every row inspected, matching or not.
            conn.execute(
                "UPDATE wake_ups SET when_cursor = ?2 WHERE wake_id = ?1",
                params![wake.wake_id, head],
            )?;
            let Ok(WakeTrigger::When(when)) = serde_json::from_str(&wake.trigger_json) else {
                crate::inbox::report_once("wake-condition-unreadable");
                return Ok(());
            };
            if matches!(
                when.fact_kind,
                InboxFactKind::WakeDue | InboxFactKind::ContextThreshold
            ) {
                return Ok(());
            }
            let (count, latest): (i64, Option<i64>) = conn.query_row(
                "SELECT COUNT(*), MAX(f.id)
                   FROM inbox_facts f
                  WHERE f.subscriber_lineage_id = ?1 AND f.id > ?2 AND f.id <= ?3
                    AND f.kind = ?4
                    AND (?5 IS NULL OR f.subject_instance_id = ?5)
                    AND NOT EXISTS (SELECT 1 FROM wake_runs r
                                     WHERE r.lineage_id = ?1
                                       AND r.child_instance_id = f.subject_instance_id)",
                params![
                    wake.lineage_id,
                    wake.when_cursor,
                    head,
                    when.fact_kind.as_str(),
                    when.subject_instance_id
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let Some(fact_id) = latest.filter(|_| count > 0) else {
                return Ok(());
            };
            let entered: String = conn.query_row(
                "SELECT state_entered_at FROM inbox_facts WHERE id = ?1",
                params![fact_id],
                |row| row.get(0),
            )?;
            fire(
                conn,
                &wake,
                Fire {
                    scheduled_at: entered,
                    due: count,
                    cause: Some((fact_id, when.fact_kind)),
                },
                now_s,
                outcome,
            )
        })?;
    }
    Ok(())
}

/// One evaluation as of `now`, in one writer transaction (queued results are
/// applied first, in their own).
pub(crate) fn tick(conn: &mut Connection, now: OffsetDateTime) -> Result<TickOutcome, StoreError> {
    drain_outcomes(conn, now);
    let now_s = rfc3339(now);
    let mut outcome = TickOutcome::default();
    let tx = immediate_tx(conn)?;
    due_scheduled(&tx, now, &now_s, &mut outcome)?;
    due_conditions(&tx, &now_s, &mut outcome)?;
    let (oldest, fresh): (Option<String>, bool) = tx.query_row(
        "SELECT (SELECT MIN(fired_at) FROM wake_runs WHERE state = 'queued'),
                EXISTS (SELECT 1 FROM wake_runs WHERE state = 'queued' AND mode = 'fresh')",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    tx.commit()?;
    outcome.oldest_waiting = oldest.as_deref().and_then(crate::inbox::parse_at);
    outcome.fresh_due |= fresh;
    Ok(outcome)
}

impl crate::store::Store {
    /// One wake evaluation as of `now`.
    pub(crate) async fn wake_tick(&self, now: OffsetDateTime) -> Result<TickOutcome, StoreError> {
        self.run_named("wake_tick", move |conn| tick(conn, now))
            .await
    }
}

/// One evaluation from the scheduler loop. A failure is logged by code and
/// retried on the next evaluation: every due occurrence stays due until its
/// run commits. Fresh runs are driven off the writer, after it.
pub(crate) async fn run_tick(state: &AppState) {
    #[cfg(any(test, feature = "test-faults"))]
    if state
        .wake
        .hold_tick
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        return;
    }
    match state.store.wake_tick(OffsetDateTime::now_utc()).await {
        Ok(outcome) => {
            state.wake.note_tick(outcome.oldest_waiting);
            if outcome.fired > 0 || outcome.missed > 0 {
                tracing::debug!(
                    category = "wake",
                    fired = outcome.fired,
                    missed = outcome.missed,
                    "wake tick"
                );
            }
            if outcome.fresh_due {
                drive_fresh(state).await;
            }
        }
        Err(err) => tracing::error!(
            category = "wake",
            code = crate::inbox::error_code(&err),
            "wake tick failed"
        ),
    }
}

// ── results ────────────────────────────────────────────────────────────────

/// Queue the result of `run_id` in the caller's transaction.
fn queue_outcome(
    conn: &Connection,
    run_id: &str,
    success: bool,
    kind: Option<&str>,
    at: &str,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO wake_outcomes (run_id, outcome, failure_kind, observed_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![run_id, if success { "success" } else { "failed" }, kind, at],
    )?;
    Ok(())
}

/// One queued result.
#[derive(Debug, Clone)]
struct QueuedOutcome {
    id: i64,
    run_id: String,
    success: bool,
    kind: Option<String>,
    at: String,
    attempts: i64,
}

/// The run a result settles.
#[derive(Debug, Clone)]
struct SettlingRun {
    wake_id: String,
    lineage_id: String,
    name: String,
    notify: String,
    state: String,
    scheduled_at: String,
    missed_before: i64,
}

/// When a result whose write failed `attempts` times is tried again.
fn retry_at(now: OffsetDateTime, attempts: i64) -> String {
    let exponent = u32::try_from(attempts.clamp(1, 16) - 1).unwrap_or(0);
    let secs = OUTCOME_RETRY_FIRST_SECS
        .saturating_mul(2_i64.saturating_pow(exponent))
        .min(OUTCOME_RETRY_MAX_SECS);
    rfc3339(now.saturating_add(time::Duration::seconds(secs)))
}

/// Apply queued results due at `now`: run rows first, then the `notify`
/// declaration. Runs on the writer connection after every writer job, on
/// every tick and once at open. A result whose write fails stays queued and
/// is tried again after a backoff (1 s, doubling, at most 60 s): only the
/// write is retried, never the run. After [`MAX_OUTCOME_ATTEMPTS`] failed
/// tries it is applied without its notice, so the run always ends.
pub(crate) fn drain_outcomes(conn: &mut Connection, now: OffsetDateTime) {
    let now_s = rfc3339(now);
    let pending = conn
        .prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM wake_outcomes
                             WHERE next_attempt_at IS NULL OR next_attempt_at <= ?1)",
        )
        .and_then(|mut stmt| stmt.query_row(params![now_s], |row| row.get::<_, bool>(0)));
    match pending {
        Ok(true) => {}
        Ok(false) => return,
        Err(err) => {
            tracing::error!(
                category = "wake",
                code = crate::inbox::error_code(&err.into()),
                "wake result probe failed"
            );
            return;
        }
    }
    let mut cursor = 0_i64;
    for _ in 0..OUTCOME_BATCHES {
        match drain_batch(conn, cursor, now) {
            Ok(Some(last)) => cursor = last,
            Ok(None) => break,
            Err(err) => {
                tracing::error!(
                    category = "wake",
                    code = crate::inbox::error_code(&err),
                    "wake result drain failed"
                );
                break;
            }
        }
    }
}

fn drain_batch(
    conn: &mut Connection,
    after: i64,
    now: OffsetDateTime,
) -> Result<Option<i64>, StoreError> {
    let tx = immediate_tx(conn)?;
    let rows: Vec<QueuedOutcome> = tx
        .prepare_cached(
            "SELECT id, run_id, outcome, failure_kind, observed_at, attempts FROM wake_outcomes
              WHERE id > ?1 AND (next_attempt_at IS NULL OR next_attempt_at <= ?3)
              ORDER BY id LIMIT ?2",
        )?
        .query_map(params![after, OUTCOME_BATCH, rfc3339(now)], |row| {
            Ok(QueuedOutcome {
                id: row.get(0)?,
                run_id: row.get(1)?,
                success: row.get::<_, String>(2)? == "success",
                kind: row.get(3)?,
                at: row.get(4)?,
                attempts: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let Some(last) = rows.last().map(|row| row.id) else {
        return Ok(None);
    };
    for row in rows {
        let applied = in_savepoint(&tx, || apply(&tx, &row, true)).or_else(|err| {
            if row.attempts + 1 >= MAX_OUTCOME_ATTEMPTS {
                // The run's own state matters more than its notice.
                in_savepoint(&tx, || apply(&tx, &row, false)).map_err(|_| err)
            } else {
                Err(err)
            }
        });
        match applied {
            Ok(()) => {
                tx.execute("DELETE FROM wake_outcomes WHERE id = ?1", params![row.id])?;
            }
            Err(err) => {
                tx.execute(
                    "UPDATE wake_outcomes SET attempts = attempts + 1, next_attempt_at = ?2
                      WHERE id = ?1",
                    params![row.id, retry_at(now, row.attempts.saturating_add(1))],
                )?;
                tracing::warn!(
                    category = "wake",
                    code = crate::inbox::error_code(&err),
                    attempts = row.attempts + 1,
                    "a wake result write failed; it is retried"
                );
            }
        }
    }
    tx.commit()?;
    Ok(Some(last))
}

fn in_savepoint(
    conn: &Connection,
    work: impl FnOnce() -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    conn.execute_batch("SAVEPOINT wake_result")?;
    match work() {
        Ok(()) => {
            conn.execute_batch("RELEASE wake_result")?;
            Ok(())
        }
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK TO wake_result; RELEASE wake_result");
            Err(err)
        }
    }
}

/// Settle the run a result names, if it is still open (the first result
/// wins), then apply its `notify` declaration.
fn apply(conn: &Connection, row: &QueuedOutcome, notify: bool) -> Result<(), StoreError> {
    #[cfg(any(test, feature = "test-faults"))]
    if notify && test_seams::take_result_failure(&row.run_id) {
        return Err(StoreError::FaultInjected("wake result write".into()));
    }
    let run: Option<SettlingRun> = conn
        .query_row(
            "SELECT wake_id, lineage_id, name, notify, state, scheduled_at, missed_before
               FROM wake_runs WHERE run_id = ?1",
            params![row.run_id],
            |r| {
                Ok(SettlingRun {
                    wake_id: r.get(0)?,
                    lineage_id: r.get(1)?,
                    name: r.get(2)?,
                    notify: r.get(3)?,
                    state: r.get(4)?,
                    scheduled_at: r.get(5)?,
                    missed_before: r.get(6)?,
                })
            },
        )
        .optional()?;
    let Some(run) = run else {
        return Ok(());
    };
    if !matches!(run.state.as_str(), "queued" | "running") {
        return Ok(());
    }
    let state = if row.success { "success" } else { "failed" };
    let kind = if row.success {
        None
    } else {
        Some(row.kind.as_deref().unwrap_or(failure::DELIVERY))
    };
    conn.execute(
        "UPDATE wake_runs SET state = ?2, ended_at = ?3, failure_kind = ?4
          WHERE run_id = ?1 AND state IN ('queued', 'running')",
        params![row.run_id, state, row.at, kind],
    )?;
    let notified = if notify {
        notify_run(conn, &row.run_id, &run, kind)?
    } else {
        false
    };
    tracing::info!(
        category = "wake",
        outcome = state,
        failure = kind.unwrap_or(""),
        notified,
        "wake run settled"
    );
    Ok(())
}

/// The chapter a lineage's notices are written from: its current chapter,
/// or the instance a lineage without continuity is.
fn notice_chapter(
    conn: &Connection,
    lineage_id: &str,
) -> Result<Option<NoticeChapter>, StoreError> {
    let current: Option<(String, i64)> = conn
        .query_row(
            "SELECT current_instance_id, generation FROM lineages WHERE lineage_id = ?1",
            params![lineage_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let current = match current {
        Some(current) => Some(current),
        None => conn
            .query_row(
                "SELECT id, generation FROM instances WHERE id = ?1",
                params![lineage_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?,
    };
    Ok(current.map(|(instance_id, generation)| NoticeChapter {
        lineage_id: lineage_id.to_owned(),
        instance_id,
        generation,
    }))
}

/// A notice title: one line, within the notice bound.
fn title(text: String) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    line.chars()
        .take(crate::owner_notify::TITLE_MAX_CHARS)
        .collect()
}

/// What a failure kind means, in the Hub's voice.
fn failure_text(kind: &str) -> &'static str {
    match kind {
        failure::NOT_RECEIVED => "the lineage does not receive wake-up facts",
        failure::NOT_DELIVERED => "it could not be delivered to the lineage",
        failure::DELIVERY => "the send to the lineage was refused",
        failure::TURN_FAILED => "the turn it started ended in an error",
        failure::CHAPTER_ENDED => "the session running it ended before its turn finished",
        failure::FRESH_REFUSED => "its fresh session could not be created",
        failure::CHILD_FAILED => "its fresh session's turn ended in an error",
        failure::CHILD_ENDED => "its fresh session ended before finishing a turn",
        _ => "it did not finish",
    }
}

/// Apply the run's `notify` declaration (§11.5) through cb-owner-notify's
/// primitive: `always` notifies on every run, `failure` on a failed one (a
/// repeat of the same failure kind pushes once until a run succeeds),
/// `never` on nothing. The run id is the dedupe key, so a retried write can
/// never claim a second notice. Returns whether a notice was claimed.
fn notify_run(
    conn: &Connection,
    run_id: &str,
    run: &SettlingRun,
    kind: Option<&str>,
) -> Result<bool, StoreError> {
    let policy: NotifyPolicy = run.notify.parse().unwrap_or_else(|_| {
        crate::inbox::report_once("wake-notify-unreadable");
        NotifyPolicy::Never
    });
    let name = crate::inbox_voice::token(&run.name);
    let missed = if run.missed_before > 0 {
        format!(
            " {} earlier occurrence{} skipped without a run of {}.",
            run.missed_before,
            if run.missed_before == 1 {
                " was"
            } else {
                "s were"
            },
            if run.missed_before == 1 {
                "its own"
            } else {
                "their own"
            }
        )
    } else {
        String::new()
    };
    let (outcome, title_text, body) = match kind {
        None => (
            WakeOutcome::Success,
            format!("Wake-up {name} ran"),
            format!(
                "Wake-up {name} ({}) ran: run {run_id}, scheduled for {}.{missed}",
                run.wake_id, run.scheduled_at
            ),
        ),
        Some(kind) => (
            WakeOutcome::Failure { kind },
            format!("Wake-up {name} failed: {kind}"),
            format!(
                "Wake-up {name} ({}) failed: {}. Run {run_id}, scheduled for {}.{missed} A \
                 repeat of this failure is not pushed again until a run succeeds.",
                run.wake_id,
                failure_text(kind),
                run.scheduled_at
            ),
        ),
    };
    let input = NoticeInput {
        chapter: notice_chapter(conn, &run.lineage_id)?,
        title: title(title_text),
        body,
        dedupe_key: Some(format!("wake-run:{run_id}")),
        source: NoticeSource::Wake,
        origin: "hub",
        device_id: None,
    };
    let notice = crate::owner_notify::notify_wake(conn, &run.wake_id, policy, outcome, &input)?;
    Ok(matches!(
        notice,
        Some(crate::owner_notify::FailureNotice::Notified(_))
    ))
}

/// At open, before any job: apply the results the previous process left
/// queued, then mark every run still `running` whose result is not already
/// queued as `interrupted` (a queued result is evidence the run ended; it is
/// applied on a later pass). An interrupted run is never replayed; its
/// wake-up's next occurrence is computed from the schedule as usual.
pub(crate) fn boot(conn: &mut Connection) {
    drain_outcomes(conn, OffsetDateTime::now_utc());
    match conn.execute(
        "UPDATE wake_runs SET state = 'interrupted', ended_at = ?1
          WHERE state = 'running'
            AND run_id NOT IN (SELECT run_id FROM wake_outcomes)",
        params![now_rfc3339()],
    ) {
        Ok(0) => {}
        Ok(runs) => tracing::info!(
            category = "wake",
            outcome = "interrupted",
            runs,
            "wake runs running at boot are interrupted, never replayed"
        ),
        Err(err) => tracing::error!(
            category = "wake",
            code = crate::inbox::error_code(&err.into()),
            "marking interrupted wake runs failed"
        ),
    }
}

/// Fold one appended journal event of `instance_id` into the results of the
/// runs it ends, in the append's transaction (the caller guards it in a
/// savepoint, so it can never harm the append):
///
/// * a fresh run's child: its first root turn end;
/// * a persistent run on this chapter: the root turn end that names the
///   run's batch (`completedTurn`), or, on a carrier that names none, the
///   next turn end after the batch's send settled delivered.
pub(crate) fn fold_turn_end(
    conn: &Connection,
    instance_id: &str,
    event: &Value,
    turn_ended: bool,
) -> Result<(), StoreError> {
    let payload = event.get("payload").unwrap_or(&Value::Null);
    let text = |pointer: &str| payload.pointer(pointer).and_then(Value::as_str);
    let root_result = text("/type") == Some("native")
        && text("/topic") == Some("turn")
        && text("/nativeName") == Some("result")
        && !crate::store::native_payload_is_subagent(payload);
    let completed = text("/relatedIds/completedTurn").filter(|id| !id.is_empty());
    let settles =
        root_result && (completed.is_some() || text("/relatedIds/settledRootTurn") == Some("true"));
    if !settles && !turn_ended {
        return Ok(());
    }
    let open: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM wake_runs
                         WHERE state = 'running' AND target_instance_id = ?1)",
        params![instance_id],
        |row| row.get(0),
    )?;
    if !open {
        return Ok(());
    }
    let status = payload
        .get("status")
        .and_then(|status| status.as_str().or_else(|| status.get("value")?.as_str()));
    let failed = status == Some("error")
        || text("/relatedIds/outcome")
            .is_some_and(|outcome| outcome.eq_ignore_ascii_case("failed"));
    let at = event
        .get("observedAt")
        .and_then(Value::as_str)
        .and_then(crate::inbox::canonical_at)
        .unwrap_or_else(now_rfc3339);
    let outcome = if failed { "failed" } else { "success" };
    conn.execute(
        "INSERT INTO wake_outcomes (run_id, outcome, failure_kind, observed_at)
         SELECT run_id, ?2, ?3, ?4 FROM wake_runs
          WHERE state = 'running' AND child_instance_id = ?1",
        params![
            instance_id,
            outcome,
            failed.then_some(failure::CHILD_FAILED),
            at
        ],
    )?;
    let kind = failed.then_some(failure::TURN_FAILED);
    match completed.filter(|_| root_result) {
        Some(command_id) => conn.execute(
            "INSERT INTO wake_outcomes (run_id, outcome, failure_kind, observed_at)
             SELECT run_id, ?2, ?3, ?4 FROM wake_runs
              WHERE state = 'running' AND target_instance_id = ?1
                AND child_instance_id IS NULL AND batch_id = ?5",
            params![instance_id, outcome, kind, at, command_id],
        )?,
        None => conn.execute(
            "INSERT INTO wake_outcomes (run_id, outcome, failure_kind, observed_at)
             SELECT r.run_id, ?2, ?3, ?4 FROM wake_runs r JOIN commands c ON c.id = r.batch_id
              WHERE r.state = 'running' AND r.target_instance_id = ?1
                AND r.child_instance_id IS NULL
                AND c.state = 'settled' AND COALESCE(c.settlement_outcome, 'completed') = 'completed'",
            params![instance_id, outcome, kind, at],
        )?,
    };
    Ok(())
}

// ── fresh mode ─────────────────────────────────────────────────────────────

/// A fresh run claimed for its create.
#[derive(Debug, Clone)]
struct FreshClaim {
    run_id: String,
    lineage_id: String,
    detail: Value,
    host_id: String,
    workspace_id: Option<String>,
    model: Option<String>,
    provider_profile_id: Option<String>,
    device: crate::store::Device,
}

/// Claim `run_id` when its lineage has a live chapter on an online host with
/// a credential to act as: the run turns `running` in this transaction,
/// before the create is attempted, so a Hub that stops mid-create finds it
/// running at boot and interrupts it rather than creating a second child.
/// `Ok(Err(category))` leaves the run queued for a later tick.
fn claim_fresh(
    conn: &mut Connection,
    run_id: &str,
) -> Result<Result<FreshClaim, &'static str>, StoreError> {
    let tx = immediate_tx(conn)?;
    let run: Option<(String, String)> = tx
        .query_row(
            "SELECT lineage_id, detail_json FROM wake_runs
              WHERE run_id = ?1 AND state = 'queued' AND mode = 'fresh'",
            params![run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((lineage_id, detail)) = run else {
        return Ok(Err("not-queued"));
    };
    let lineage: Option<(String, String)> = tx
        .query_row(
            "SELECT current_instance_id, state FROM lineages WHERE lineage_id = ?1",
            params![lineage_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let chapter_id = match lineage {
        Some((_, state)) if state != "running" => return Ok(Err("no-live-chapter")),
        Some((current, _)) => current,
        None => lineage_id.clone(),
    };
    #[allow(clippy::type_complexity)]
    let chapter: Option<(
        String,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        String,
    )> = tx
        .query_row(
            "SELECT host_id, workspace_id, lifecycle, ended_at, fenced_at, spec_json
               FROM instances WHERE id = ?1",
            params![chapter_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()?;
    let Some((host_id, workspace_id, lifecycle, ended_at, fenced_at, spec)) = chapter else {
        return Ok(Err("no-live-chapter"));
    };
    if ended_at.is_some()
        || fenced_at.is_some()
        || !matches!(lifecycle.as_str(), "ready" | "running")
    {
        return Ok(Err("no-live-chapter"));
    }
    let online: bool = tx
        .query_row(
            "SELECT state = 'online' FROM hosts WHERE id = ?1",
            params![host_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(false);
    if !online {
        return Ok(Err("host-offline"));
    }
    // The chapter's own Agent credential: the create is the chapter's.
    let device: Option<(String, String)> = tx
        .query_row(
            "SELECT id, name FROM devices WHERE kind = 'agent' AND instance_id = ?1
              ORDER BY created_at DESC, id DESC LIMIT 1",
            params![chapter_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((device_id, device_name)) = device else {
        return Ok(Err("no-credential"));
    };
    let claimed = tx.execute(
        "UPDATE wake_runs SET state = 'running', started_at = ?2
          WHERE run_id = ?1 AND state = 'queued'",
        params![run_id, now_rfc3339()],
    )?;
    if claimed == 0 {
        return Ok(Err("not-queued"));
    }
    tx.commit()?;
    let spec: Value = serde_json::from_str(&spec).unwrap_or(Value::Null);
    let field = |name: &str| {
        spec.get(name)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    Ok(Ok(FreshClaim {
        run_id: run_id.to_owned(),
        lineage_id,
        detail: serde_json::from_str(&detail).unwrap_or(Value::Null),
        host_id,
        workspace_id,
        model: field("model"),
        provider_profile_id: field("providerProfileId"),
        device: crate::store::Device {
            id: device_id,
            name: device_name,
            kind: "agent".into(),
            instance_id: Some(chapter_id),
        },
    }))
}

/// Create the child of every fresh run that is due and whose lineage can
/// act now. Serialized; a run claimed by an earlier drive is not claimed
/// again.
pub(crate) async fn drive_fresh(state: &AppState) {
    let Ok(_drive) = state.wake.fresh.try_lock() else {
        return;
    };
    let runs = state
        .store
        .read("wake_fresh_due", |conn| {
            Ok(conn
                .prepare_cached(
                    "SELECT run_id FROM wake_runs WHERE state = 'queued' AND mode = 'fresh'
                      ORDER BY fired_at, run_id LIMIT ?1",
                )?
                .query_map(params![FRESH_PER_DRIVE], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await;
    let runs = match runs {
        Ok(runs) => runs,
        Err(err) => {
            tracing::error!(
                category = "wake",
                code = crate::inbox::error_code(&err),
                "fresh runs could not be read"
            );
            return;
        }
    };
    for run_id in runs {
        let claimed = state
            .store
            .run_named("wake_fresh_claim", {
                let run_id = run_id.clone();
                move |conn| claim_fresh(conn, &run_id)
            })
            .await;
        match claimed {
            Ok(Ok(claim)) => create_fresh(state, claim).await,
            Ok(Err(hold)) => tracing::debug!(category = "wake", hold, "fresh run held"),
            Err(err) => tracing::error!(
                category = "wake",
                code = crate::inbox::error_code(&err),
                "fresh run claim failed"
            ),
        }
    }
}

/// The child's create: an ordinary Agent create, the chapter's own, on its
/// host, briefed with the envelope and the wake-up's text.
async fn create_fresh(state: &AppState, claim: FreshClaim) {
    let text = claim
        .detail
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("");
    let name = claim
        .detail
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("wake");
    let mut body = json!({
        "kind": "claude",
        "driver": FRESH_DRIVER,
        "hostId": claim.host_id,
        "title": crate::inbox_voice::clip_head(&format!("wake {name}")),
        "prompt": crate::inbox_voice::fresh_brief(&claim.lineage_id, &claim.detail, text),
    });
    if let Some(workspace) = &claim.workspace_id {
        body["workspaceId"] = json!(workspace);
    }
    if let Some(model) = &claim.model {
        body["model"] = json!(model);
    }
    if let Some(profile) = &claim.provider_profile_id {
        body["providerProfileId"] = json!(profile);
    }
    let behalf = crate::http::OnBehalf {
        wake_run_id: claim.run_id.clone(),
        root_turn: remuda_protocol::RootTurn {
            command_id: claim.run_id.clone(),
            origin: InputOrigin::Hub,
            continuation: remuda_protocol::RootTurnContinuation::Hub,
        },
    };
    let created = crate::http::create_on_behalf(state, claim.device, body, behalf).await;
    let run_id = claim.run_id;
    match created {
        Ok(value) => {
            let child = value
                .pointer("/instance/instanceId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            // The create's own transaction recorded the child; this only
            // covers a create path that did not.
            if let Some(child) = child {
                let linked = state
                    .store
                    .run_named("wake_fresh_link", move |conn| {
                        conn.execute(
                            "UPDATE wake_runs SET child_instance_id = ?2, target_instance_id = ?2
                              WHERE run_id = ?1 AND state = 'running'
                                AND child_instance_id IS NULL",
                            params![run_id, child],
                        )?;
                        Ok(())
                    })
                    .await;
                if let Err(err) = linked {
                    tracing::error!(
                        category = "wake",
                        code = crate::inbox::error_code(&err),
                        "fresh child link failed"
                    );
                }
            }
            tracing::info!(category = "wake", outcome = "fresh-created");
        }
        Err(error) => {
            let (status, body) = error.status_code_and_body();
            let code = body
                .get("reason")
                .or_else(|| body.get("code"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let refused = state
                .store
                .run_named("wake_fresh_refused", move |conn| {
                    queue_outcome(
                        conn,
                        &run_id,
                        false,
                        Some(failure::FRESH_REFUSED),
                        &now_rfc3339(),
                    )
                })
                .await;
            if let Err(err) = refused {
                tracing::error!(
                    category = "wake",
                    code = crate::inbox::error_code(&err),
                    "fresh refusal could not be queued"
                );
            }
            tracing::info!(
                category = "wake",
                outcome = "fresh-refused",
                status,
                code = code.as_str()
            );
        }
    }
}

/// Test seams: fault injection for the result write, keyed by run id so
/// tests running in parallel never disarm each other.
#[cfg(any(test, feature = "test-faults"))]
pub(crate) mod test_seams {
    use std::collections::HashMap;
    use std::sync::Mutex;

    static RESULT_FAILURES: Mutex<Option<HashMap<String, u32>>> = Mutex::new(None);

    /// Make the next `times` full result writes for `run_id` fail; 0 disarms.
    pub fn arm_result_failure(run_id: &str, times: u32) {
        let mut slot = RESULT_FAILURES
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let armed = slot.get_or_insert_with(HashMap::new);
        if times == 0 {
            armed.remove(run_id);
        } else {
            armed.insert(run_id.to_owned(), times);
        }
    }

    /// Whether this result write for `run_id` is an armed failure (counts it).
    pub(crate) fn take_result_failure(run_id: &str) -> bool {
        let mut slot = RESULT_FAILURES
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let Some(armed) = slot.as_mut() else {
            return false;
        };
        match armed.get_mut(run_id) {
            Some(times) => {
                *times -= 1;
                if *times == 0 {
                    armed.remove(run_id);
                }
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(raw: &str) -> OffsetDateTime {
        crate::inbox::parse_at(raw).expect("time")
    }

    #[test]
    fn an_at_fires_once_and_is_done() {
        let due = due_occurrences(
            "at",
            at("2026-11-04T10:00:00.000Z"),
            None,
            at("2026-11-04T10:00:03.000Z"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(due.count, 1);
        assert_eq!(due.next, None);
        assert_eq!(
            due_occurrences(
                "at",
                at("2026-11-04T10:00:00.000Z"),
                None,
                at("2026-11-04T09:59:59.000Z")
            ),
            Ok(None)
        );
    }

    #[test]
    fn missed_every_occurrences_fire_once_for_the_latest() {
        // Due at 10:00 every 3 min; the Hub looks at 10:10: 10:00, 10:03,
        // 10:06 and 10:09 are due, 10:09 fires, 10:12 is next.
        let due = due_occurrences(
            "every",
            at("2026-11-04T10:00:00.000Z"),
            Some(180),
            at("2026-11-04T10:10:00.000Z"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(due.count, 4);
        assert_eq!(rfc3339(due.latest), "2026-11-04T10:09:00.000Z");
        assert_eq!(
            due.next.map(rfc3339).as_deref(),
            Some("2026-11-04T10:12:00.000Z")
        );
    }

    #[test]
    fn a_stored_interval_out_of_range_is_a_category_never_a_panic() {
        let next = at("2026-11-04T10:00:00.000Z");
        let now = at("2026-11-04T10:10:00.000Z");
        for every in [Some(i64::MAX), Some(i64::MIN), Some(0), Some(-1), None] {
            assert_eq!(
                due_occurrences("every", next, every, now),
                Err("wake-interval-out-of-range"),
                "{every:?}"
            );
        }
        // The largest accepted interval near the end of the representable
        // range overflows the next time: a category too.
        let far = OffsetDateTime::new_utc(
            time::Date::from_calendar_date(9999, time::Month::December, 31).unwrap(),
            time::Time::MIDNIGHT,
        );
        assert_eq!(
            due_occurrences("every", far, Some(WAKE_MAX_SECS), far),
            Err("wake-schedule-overflow")
        );
        assert_eq!(
            due_occurrences("sometimes", next, Some(60), now),
            Err("wake-trigger-unknown")
        );
    }

    #[test]
    fn a_failed_result_write_backs_off_to_a_bound() {
        let now = at("2026-11-04T10:00:00.000Z");
        assert_eq!(retry_at(now, 1), "2026-11-04T10:00:01.000Z");
        assert_eq!(retry_at(now, 2), "2026-11-04T10:00:02.000Z");
        assert_eq!(retry_at(now, 7), "2026-11-04T10:01:00.000Z");
        assert_eq!(retry_at(now, i64::MAX), "2026-11-04T10:01:00.000Z");
        let total: i64 = (1..MAX_OUTCOME_ATTEMPTS)
            .map(|n| (at(&retry_at(now, n)) - now).whole_seconds())
            .sum();
        assert!(
            total >= 120,
            "minutes of retries before the notice is dropped: {total}"
        );
    }

    fn request(trigger: Value) -> WakeAddRequest {
        serde_json::from_value(json!({
            "name": "gate-check", "text": "Check the gate queue.", "trigger": trigger,
            "notify": "failure"
        }))
        .unwrap()
    }

    #[test]
    fn an_add_is_bounded_and_resolves_its_trigger() {
        let now = at("2026-11-04T10:00:00.000Z");
        let checked = check_add(&request(json!({"kind": "at", "afterSecs": 120})), now).unwrap();
        assert_eq!(
            checked.first_fire.as_deref(),
            Some("2026-11-04T10:02:00.000Z")
        );
        assert_eq!(
            checked.trigger,
            WakeTrigger::At(WakeAt {
                at: Some("2026-11-04T10:02:00.000Z".into()),
                after_secs: None
            })
        );
        // The same request a minute later matches as a repeat.
        let later = check_add(
            &request(json!({"kind": "at", "afterSecs": 120})),
            at("2026-11-04T10:01:00.000Z"),
        )
        .unwrap();
        assert_eq!(later.request_json, checked.request_json);
        for bad in [
            json!({"kind": "at"}),
            json!({"kind": "at", "at": "2026-11-04T10:00:00Z", "afterSecs": 1}),
            json!({"kind": "at", "afterSecs": -1}),
            json!({"kind": "at", "afterSecs": i64::MAX}),
            json!({"kind": "at", "at": "2099-01-01T00:00:00Z"}),
            json!({"kind": "every", "everySecs": i64::MAX}),
            json!({"kind": "every", "everySecs": 59}),
            json!({"kind": "when", "factKind": "wake-due"}),
        ] {
            assert!(
                matches!(
                    check_add(&request(bad.clone()), now),
                    Err(WakeError::BadRequest(_))
                ),
                "{bad}"
            );
        }
        assert!(matches!(
            check_add(
                &request(json!({"kind": "when", "factKind": "context-threshold"})),
                now
            ),
            Err(WakeError::Unsupported {
                reason: "condition-unsupported",
                ..
            })
        ));
        let mut with_check = request(json!({"kind": "at", "afterSecs": 1}));
        with_check.check = Some(json!({"command": "true"}));
        assert!(matches!(
            check_add(&with_check, now),
            Err(WakeError::Unsupported {
                reason: "check-unsupported",
                ..
            })
        ));
        let mut long = request(json!({"kind": "at", "afterSecs": 1}));
        long.text = "x".repeat(WAKE_TEXT_MAX_CHARS + 1);
        assert!(matches!(
            check_add(&long, now),
            Err(WakeError::BadRequest(_))
        ));
        for name in ["two words", "a/b", "wak_0193a5b2", "..", &"n".repeat(65)] {
            let mut odd = request(json!({"kind": "at", "afterSecs": 1}));
            odd.name = Some(name.to_owned());
            assert!(
                matches!(check_add(&odd, now), Err(WakeError::BadRequest(_))),
                "{name}"
            );
        }
    }

    #[test]
    fn a_credential_in_the_text_is_refused_as_one() {
        let now = at("2026-11-04T10:00:00.000Z");
        let mut secret = request(json!({"kind": "at", "afterSecs": 1}));
        secret.text = format!(
            "use {} for the gateway",
            ["sk", "-ant-api03-", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"].concat()
        );
        // Too long as well: the lint still decides first.
        secret.text.push_str(&"x".repeat(WAKE_TEXT_MAX_CHARS));
        assert!(matches!(
            check_add(&secret, now),
            Err(WakeError::SecretRejected { field: "text", .. })
        ));
    }
}
