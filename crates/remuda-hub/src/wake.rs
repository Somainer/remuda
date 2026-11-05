//! Durable wake-ups (cb-wake-engine; D-060 §11, main-agent.md §11.1, §11.3,
//! §11.4, §11.9).
//!
//! A lineage (or the owner, for any lineage) adds wake-ups with
//! `remuda wake add|list|rm`. The Hub keeps them in `hub.sqlite`, claims each
//! due occurrence once on the writer thread, and records a run:
//!
//! * **Rows (§11.1).** `wake_ups`: the lineage, a name unique in it (so an add
//!   under the same name is idempotent), the trigger (`at`, `every`, `when`),
//!   the optional check with its admission (cb-wake-check, below), the mode,
//!   the declared `notify`, the state, the next fire time, the missed count,
//!   the creator and the last real run. `wake_runs`: the wake-up, the
//!   scheduled time, the state, start and end, the run's check (what it ran,
//!   where, its state, exit code, summary and bounded stdout), the inbox fact
//!   and batch that carried it, a fresh run's child, and the occurrences
//!   missed before it. Idempotent DDL, so `scripts/hub-backup.sh` carries
//!   them whole.
//! * **The run state machine.** `queued` → `running` → `success`, `failed`
//!   or `skipped` (its check exited 2). A persistent run is `queued`
//!   while its `wake-due` fact waits in the inbox, `running` from the moment
//!   its batch's forward intent is marked until the turn that batch started
//!   ends; a fresh run is `running` from the moment the Hub starts creating
//!   its child until the child's first turn ends. At boot every `running` run
//!   becomes `interrupted` and is never replayed (its fact stays consumed
//!   even if the batch is later released); the next occurrence is computed
//!   from the schedule as usual. Boot first applies what the previous
//!   process left queued (check reports in `wake_check_reports`, then results
//!   in `wake_outcomes`). The exception is a run with a result or a check
//!   report still queued after that (its write kept failing): that is
//!   evidence the run ended or its check answered, so the run stays
//!   `running` until a later drain writes it. A run whose check was claimed
//!   and never answered is `interrupted` with its check, and never asked
//!   again.
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
//! * **Checks (cb-wake-check, main-agent.md §11.2, §11.9).** A wake-up may
//!   carry a shell command one Node runs before each run, under a mandatory
//!   timeout. It is bounded and secret-linted at add time, placed on a host
//!   in the lineage's scope where the lineage has a workspace, and admitted
//!   on the OA1 axis (the owner's own add; an Agent on its own host while it
//!   runs with its permission control off; otherwise the owner's one-shot
//!   approval). A run waits for its check (`pending` while the host is
//!   offline or the Node never received the call), is claimed `running`
//!   before the Node is asked (`wake.check`), and routes by the answer: exit
//!   0 runs it with the check's stdout as its quoted summary, exit 2 skips
//!   it, anything else fails it with a `wake-failed` fact. The Node's report
//!   is queued in `wake_check_reports` and applied with retried writes.
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
    InboxFactKind, InputOrigin, WAKE_CHECK_COMMAND_MAX_CHARS, WAKE_CHECK_TIMEOUT_MAX_SECS,
    WAKE_CHECK_TIMEOUT_MIN_SECS, WAKE_EVERY_MIN_SECS, WAKE_MAX_PER_LINEAGE, WAKE_MAX_SECS,
    WAKE_NAME_MAX_CHARS, WAKE_RUNS_LISTED, WAKE_TEXT_MAX_CHARS, WakeAddRequest, WakeAddResult,
    WakeAt, WakeCheck, WakeCheckAdmission, WakeCheckRequest, WakeCheckState, WakeCreator,
    WakeEvery, WakeList, WakeMode, WakeNotify, WakeRemoved, WakeRun, WakeRunCheck, WakeRunState,
    WakeTrigger, WakeUp, WakeUpState,
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
/// Checks one drive claims at most.
const CHECKS_PER_DRIVE: i64 = 16;
/// How much longer than a check's own timeout plus the Node's bounded
/// overhead (`WAKE_CHECK_NODE_OVERHEAD_SECS`) the Hub waits for its answer:
/// the link and the reply.
const CHECK_RPC_MARGIN_SECS: u64 = 30;
/// The first delay before a check its Node never received is asked again; it
/// doubles per attempt up to [`CHECK_RETRY_MAX_SECS`].
const CHECK_RETRY_FIRST_SECS: i64 = 1;
/// The longest delay between two attempts to reach a check's Node.
const CHECK_RETRY_MAX_SECS: i64 = 60;

/// How long the Hub waits for a Node's answer to a check of `timeout_secs`:
/// longer than the Node can take (the check's timeout plus its bounded
/// overhead), so a check that ran its whole timeout is never taken as lost
/// while it still runs.
pub(crate) fn check_rpc_wait(timeout_secs: u64) -> std::time::Duration {
    std::time::Duration::from_secs(
        timeout_secs
            .saturating_add(remuda_protocol::WAKE_CHECK_NODE_OVERHEAD_SECS)
            .saturating_add(CHECK_RPC_MARGIN_SECS),
    )
}

/// The delay before the next attempt to reach a check's Node, after
/// `attempts` that never reached it: 1 s, doubling, at most 60 s.
fn check_retry_secs(attempts: i64) -> i64 {
    let exponent = u32::try_from(attempts.clamp(1, 16) - 1).unwrap_or(0);
    CHECK_RETRY_FIRST_SECS
        .saturating_mul(2_i64.saturating_pow(exponent))
        .min(CHECK_RETRY_MAX_SECS)
}

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
    /// The check exited with a code other than 0 or 2, or a signal ended it.
    pub(crate) const CHECK_EXIT: &str = "check-exit";
    /// The check ran into its timeout; its process group was killed.
    pub(crate) const CHECK_TIMEOUT: &str = "check-timeout";
    /// `sh -n` refused the check: nothing ran.
    pub(crate) const CHECK_SYNTAX: &str = "check-syntax";
    /// The check could not start on its Node (the Node refused the call, its
    /// working directory is unusable, or the stored check is unreadable).
    pub(crate) const CHECK_UNAVAILABLE: &str = "check-unavailable";
    /// The lineage has no workspace on the check's host: nothing ran (r2;
    /// a check never runs in a workspace its Node picks).
    pub(crate) const CHECK_NO_WORKSPACE: &str = "check-no-workspace";
    /// The lineage's workspace is there, the check's directory in it is not
    /// usable: nothing ran (r3).
    pub(crate) const CHECK_CWD_UNUSABLE: &str = "check-cwd-unusable";
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
    // cb-wake-check (§11.2): the wake-up's last real run, and each run's
    // check: what it runs, where, its state, and how it exited.
    use crate::store::ensure_column;
    ensure_column(conn, "wake_ups", "last_run_at", "TEXT")?;
    ensure_column(conn, "wake_runs", "check_json", "TEXT")?;
    ensure_column(conn, "wake_runs", "check_state", "TEXT")?;
    ensure_column(conn, "wake_runs", "check_host_id", "TEXT")?;
    ensure_column(conn, "wake_runs", "check_summary", "TEXT")?;
    ensure_column(conn, "wake_runs", "check_output", "TEXT")?;
    ensure_column(
        conn,
        "wake_runs",
        "check_output_truncated",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    // r3: how the check's processes were contained, as its Node reported.
    ensure_column(conn, "wake_runs", "check_containment", "TEXT")?;
    ensure_column(
        conn,
        "wake_runs",
        "check_containment_incomplete",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    // r2: attempts that never reached the check's Node, and when to try again.
    ensure_column(
        conn,
        "wake_runs",
        "check_attempts",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(conn, "wake_runs", "check_next_at", "TEXT")?;
    // r2: a Node's report on a check, waiting to be applied (§11.1: only the
    // write is retried, never the run).
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS wake_check_reports (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            run_id TEXT NOT NULL,
            report_json TEXT NOT NULL,
            observed_at TEXT NOT NULL,
            attempts INTEGER NOT NULL DEFAULT 0,
            next_attempt_at TEXT
         );",
    )?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS wake_runs_check
            ON wake_runs(check_state) WHERE check_state IN ('pending', 'running');",
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
    /// Checks waiting for their Node's answer, and a signal when one is
    /// settled (a test waits for none to be left).
    checks_in_flight: Arc<std::sync::atomic::AtomicUsize>,
    check_settled: Arc<tokio::sync::Notify>,
    /// Test seam: while set, the scheduler's ticks are skipped.
    #[cfg(any(test, feature = "test-faults"))]
    pub(crate) hold_tick: Arc<std::sync::atomic::AtomicBool>,
    /// Test seam: while set, a check's answer is dropped unwritten, as if
    /// the Hub had stopped between the claim and the answer (a crash).
    #[cfg(any(test, feature = "test-faults"))]
    pub(crate) drop_check_answers: Arc<std::sync::atomic::AtomicBool>,
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

    /// Test seam: wait until no check is waiting for its Node's answer.
    #[cfg(any(test, feature = "test-faults"))]
    pub(crate) async fn checks_settled(&self) {
        use std::sync::atomic::Ordering;
        loop {
            let settled = self.check_settled.notified();
            tokio::pin!(settled);
            settled.as_mut().enable();
            if self.checks_in_flight.load(Ordering::SeqCst) == 0 {
                return;
            }
            settled.await;
        }
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
                             fact kind), notify never, failure or always, and for a check a \
                             command of at most {WAKE_CHECK_COMMAND_MAX_CHARS} characters, \
                             timeoutSecs {WAKE_CHECK_TIMEOUT_MIN_SECS}..=\
                             {WAKE_CHECK_TIMEOUT_MAX_SECS} (required) and hostId (required \
                             for the owner)"
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
    /// The optional check, bounded; its host is resolved by the route.
    pub(crate) check: Option<CheckedCheck>,
    /// The request as sent, canonical: what a repeat must match.
    pub(crate) request_json: String,
}

/// A bounded check, before its host is resolved and its admission decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckedCheck {
    pub(crate) command: String,
    /// `None`: an Agent's own host (the owner must name one).
    pub(crate) host_id: Option<String>,
    pub(crate) timeout_secs: i64,
}

/// Bound a check: a command of at most [`WAKE_CHECK_COMMAND_MAX_CHARS`]
/// characters, a host id, and the mandatory timeout (no default: a hung
/// check would stall the wake-up for good).
fn check_check(check: &WakeCheckRequest) -> Result<CheckedCheck, WakeError> {
    let command = check.command.as_deref().map(str::trim).unwrap_or("");
    if command.is_empty() {
        return Err(WakeError::BadRequest(
            "check.command is required: the shell command the check runs".into(),
        ));
    }
    let chars = command.chars().count();
    if chars > WAKE_CHECK_COMMAND_MAX_CHARS {
        return Err(WakeError::BadRequest(format!(
            "check.command is {chars} characters; at most {WAKE_CHECK_COMMAND_MAX_CHARS}"
        )));
    }
    if command.contains('\0') {
        return Err(WakeError::BadRequest(
            "check.command must not contain a NUL character".into(),
        ));
    }
    let Some(timeout_secs) = check.timeout_secs else {
        return Err(WakeError::BadRequest(format!(
            "check.timeoutSecs is required: there is no default, a hung check would stall the \
             wake-up; give {WAKE_CHECK_TIMEOUT_MIN_SECS}..={WAKE_CHECK_TIMEOUT_MAX_SECS} seconds"
        )));
    };
    if !(WAKE_CHECK_TIMEOUT_MIN_SECS..=WAKE_CHECK_TIMEOUT_MAX_SECS).contains(&timeout_secs) {
        return Err(WakeError::BadRequest(format!(
            "check.timeoutSecs must be between {WAKE_CHECK_TIMEOUT_MIN_SECS} and \
             {WAKE_CHECK_TIMEOUT_MAX_SECS}"
        )));
    }
    let host_id = match check.host_id.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(host) if plain_token(host, 96) => Some(host.to_owned()),
        Some(_) => {
            return Err(WakeError::BadRequest(
                "check.hostId must be a host id".into(),
            ));
        }
    };
    Ok(CheckedCheck {
        command: command.to_owned(),
        host_id,
        timeout_secs,
    })
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
        (
            "check.command",
            request
                .check
                .as_ref()
                .and_then(|check| check.command.as_deref()),
        ),
    ] {
        if let Some(rule) = text.and_then(crate::secret_lint::first_secret) {
            return Err(WakeError::SecretRejected { field, rule });
        }
    }
    let check = request.check.as_ref().map(check_check).transpose()?;
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
                // A wake-up's own runs record these: firing on them could
                // keep a wake-up waking itself.
                InboxFactKind::WakeDue | InboxFactKind::WakeFailed => {
                    return Err(WakeError::BadRequest(format!(
                        "a wake-up cannot fire on {} facts",
                        when.fact_kind.as_str()
                    )));
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
    let mut request_json = json!({
        "text": text,
        "trigger": request.trigger,
        "mode": mode,
        "notify": request.notify,
    });
    // A different command (or host, or timeout) under the same name is a
    // different wake-up: changing a check needs a new add, never an edit.
    if let Some(check) = &check {
        request_json["check"] = json!({
            "command": check.command,
            "hostId": check.host_id,
            "timeoutSecs": check.timeout_secs,
        });
    }
    Ok(CheckedAdd {
        name,
        text: text.to_owned(),
        trigger,
        first_fire,
        every_secs,
        mode,
        notify: request.notify,
        check,
        request_json: request_json.to_string(),
    })
}

// ── rows ───────────────────────────────────────────────────────────────────

const WAKE_COLUMNS: &str = "wake_id, lineage_id, name, text, trigger_json, check_json, mode, \
     notify, state, next_fire_at, missed_count, created_at, updated_at, last_fired_at, \
     creator_origin, creator_instance_id, last_run_at";

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
        last_run_at: row.get(16)?,
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
     missed_before, failure_kind, check_state, check_host_id, check_summary, check_output, \
     check_output_truncated, check_containment, check_containment_incomplete";

fn run_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WakeRun> {
    let mode: String = row.get(4)?;
    let state: String = row.get(7)?;
    let check_state: Option<String> = row.get(16)?;
    let check = match check_state {
        Some(check_state) => Some(WakeRunCheck {
            state: WakeCheckState::parse(&check_state).unwrap_or(WakeCheckState::Failed),
            host_id: row.get::<_, Option<String>>(17)?.unwrap_or_default(),
            summary: row.get(18)?,
            output: row.get(19)?,
            output_truncated: row.get::<_, i64>(20)? != 0,
            containment: row
                .get::<_, Option<String>>(21)?
                .and_then(|raw| serde_json::from_value(Value::String(raw)).ok()),
            containment_incomplete: row.get::<_, i64>(22)? != 0,
        }),
        None => None,
    };
    Ok(WakeRun {
        check,
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
    /// The caller as it acts (a Human token narrowed to a chapter is that
    /// chapter's Agent): what a check's admission is decided on.
    pub(crate) device: crate::store::Device,
}

/// Whether `host_id` is a Node in the lineage's scope: an enrolled host the
/// lineage's current chapter may place work on (its delegation scope, as
/// host facts read it, §10.3).
pub(crate) fn host_in_scope(
    conn: &Connection,
    lineage_id: &str,
    host_id: &str,
) -> Result<bool, StoreError> {
    let enrolled: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM hosts WHERE id = ?1)",
        params![host_id],
        |row| row.get(0),
    )?;
    if !enrolled {
        return Ok(false);
    }
    let scope: Option<Option<String>> = conn
        .query_row(
            "SELECT scope_json FROM instances
              WHERE id = COALESCE((SELECT current_instance_id FROM lineages
                                    WHERE lineage_id = ?1), ?1)",
            params![lineage_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(scope) = scope else {
        return Ok(false);
    };
    let scope = scope
        .and_then(|raw| serde_json::from_str::<remuda_protocol::InstanceScope>(&raw).ok())
        .unwrap_or_default();
    Ok(scope.allows_host(host_id))
}

/// What the route reads before it decides a check's admission.
#[derive(Debug)]
struct CheckProbe {
    /// The same add under the same name is already stored: nothing will be
    /// added, so nothing is admitted.
    repeat: bool,
    /// A different wake-up holds the name: the add is refused anyway, so the
    /// owner is not paged for it.
    taken: bool,
    /// The host the check runs on, or why it cannot.
    host: Result<String, CheckHostRefusal>,
    /// The Agent caller's own host.
    caller_host: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckHostRefusal {
    /// The owner did not name the host.
    Required,
    /// Not a Node in the lineage's scope.
    OutOfScope,
    /// The lineage has no workspace on that host.
    NoWorkspace,
}

fn probe_check(
    conn: &Connection,
    lineage_id: &str,
    chapter: Option<&str>,
    requested: Option<&str>,
    name: Option<&str>,
    request_json: &str,
) -> Result<CheckProbe, StoreError> {
    let stored: Option<String> = match name {
        Some(name) => conn
            .query_row(
                "SELECT request_json FROM wake_ups WHERE lineage_id = ?1 AND name = ?2",
                params![lineage_id, name],
                |row| row.get(0),
            )
            .optional()?,
        None => None,
    };
    let repeat = stored.as_deref() == Some(request_json);
    let taken = stored.is_some() && !repeat;
    let caller_host: Option<String> = match chapter {
        Some(chapter) => conn
            .query_row(
                "SELECT host_id FROM instances WHERE id = ?1",
                params![chapter],
                |row| row.get(0),
            )
            .optional()?,
        None => None,
    };
    let host = match requested.map(str::to_owned).or_else(|| caller_host.clone()) {
        None => Err(CheckHostRefusal::Required),
        Some(host) if !host_in_scope(conn, lineage_id, &host)? => Err(CheckHostRefusal::OutOfScope),
        Some(host) if check_place(conn, lineage_id, &host)?.is_none() => {
            Err(CheckHostRefusal::NoWorkspace)
        }
        Some(host) => Ok(host),
    };
    Ok(CheckProbe {
        repeat,
        taken,
        host,
        caller_host,
    })
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
    /// D-062 r3 item 5: the lineage is a claimed herdr pane.
    ClaimedPane,
    /// The check's host left the lineage's scope.
    OutOfScope,
    /// A check reached the writer unadmitted: only an exact repeat may, and
    /// the stored wake-up it repeated is gone.
    NeedsAdmission,
}

/// One add, in one writer transaction. `admitted` is the check's resolved
/// host and admission; `None` with a check only for an exact repeat, which
/// adds nothing.
pub(crate) fn add(
    conn: &mut Connection,
    caller: &WakeCaller,
    checked: &CheckedAdd,
    admitted: Option<&WakeCheck>,
) -> Result<Added, StoreError> {
    let tx = immediate_tx(conn)?;
    // D-057 §7.3: a fenced, superseded or paused chapter writes nothing.
    let (initiator, device_id) = caller.authority.as_check();
    check_initiator(&tx, initiator, device_id)?;
    let lineage = caller.lineage_id.as_str();
    if !lineage_exists(&tx, lineage)? {
        return Ok(Added::NoLineage);
    }
    // D-062 r3 item 5: neither a persistent run (an inbox batch) nor a fresh
    // child on the chapter's behalf may come from a claimed pane's wake-up.
    if crate::herdr_claims::claimed_lineage(&tx, lineage)? {
        return Ok(Added::ClaimedPane);
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
    let check_json = match (&checked.check, admitted) {
        (None, _) => None,
        (Some(_), None) => return Ok(Added::NeedsAdmission),
        (Some(_), Some(check)) => {
            if !host_in_scope(&tx, lineage, &check.host_id)? {
                return Ok(Added::OutOfScope);
            }
            Some(serde_json::to_string(check)?)
        }
    };
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
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?17, ?8, ?9, 'active', ?10, 0, ?11, ?12,
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
            check_json,
        ],
    )?;
    // The audit row names who added what kind of wake-up, and how its check
    // was admitted (the decision is recorded); the name, the text and the
    // command stay in the row.
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
                "check": admitted.map(|check| json!({
                    "hostId": check.host_id,
                    "timeoutSecs": check.timeout_secs,
                    "admission": check.admission.as_str(),
                    "approvalId": check.approval_id,
                })),
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
                device,
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
                device,
            })
        }
        _ => Err(not_the_lineage()),
    }
}

/// §11.2: a check is a shell command that does not pass through the
/// harness's own permission control, so it is admitted on the OA1 axis.
///
/// * The owner (a Human device) adding one is the owner's own act: admitted,
///   on the host the owner names.
/// * An Agent adds one without approval only when it runs on the Agent's own
///   host and the Agent itself runs with its harness's permission control
///   off (OA1's "never beyond the creator").
/// * Any other check (another host, or an Agent that keeps its control on)
///   needs the owner's D-017 one-shot approval of this exact check, before
///   anything is stored. An approved retry of the same check passes; a
///   changed command, host or timeout is a new request.
///
/// `None` when the same add under the same name is already stored: nothing
/// is added, so nothing is admitted.
async fn admit_check(
    state: &AppState,
    headers: &HeaderMap,
    caller: &WakeCaller,
    checked: &CheckedAdd,
    check: &CheckedCheck,
) -> Result<Option<WakeCheck>, WakeError> {
    let probe = state
        .store
        .read("wake_check_probe", {
            let lineage = caller.lineage_id.clone();
            let chapter = (caller.origin == InputOrigin::Agent)
                .then(|| caller.device.instance_id.clone())
                .flatten();
            // The owner names the host (the owner's device has no chapter,
            // so nothing defaults it): the Hub never picks one for the owner.
            // An Agent's omitted host is its own.
            let requested = check.host_id.clone();
            let name = checked.name.clone();
            let request_json = checked.request_json.clone();
            move |conn| {
                probe_check(
                    conn,
                    &lineage,
                    chapter.as_deref(),
                    requested.as_deref(),
                    name.as_deref(),
                    &request_json,
                )
            }
        })
        .await
        .map_err(map_store)?;
    if probe.repeat {
        return Ok(None);
    }
    if probe.taken {
        return Err(name_taken());
    }
    let host_id = match probe.host {
        Ok(host) => host,
        Err(CheckHostRefusal::Required) => {
            return Err(WakeError::BadRequest(
                "check.hostId is required when the owner adds a check: name the Node it runs \
                 on"
                .into(),
            ));
        }
        // A host outside the lineage's scope is refused before anyone is
        // paged.
        Err(CheckHostRefusal::OutOfScope) => return Err(HubError::out_of_scope().into()),
        // r2: knowable now, so refused now; nobody is paged for it.
        Err(CheckHostRefusal::NoWorkspace) => {
            return Err(WakeError::Conflict {
                reason: "check-no-workspace",
                message: "the lineage has no workspace on that host: its chapter is not there \
                          and its project has no member workspace there"
                    .into(),
                hint: "name a host where the lineage has a workspace, or ask the owner to add \
                       the host's workspace to the project",
            });
        }
    };
    let (admission, approval_id) = match caller.origin {
        InputOrigin::Human => (WakeCheckAdmission::Owner, None),
        _ => {
            let own_host = probe.caller_host.as_deref() == Some(host_id.as_str());
            let creator =
                crate::agent_scope::creator_permission_mode(state, &caller.device).await?;
            let control_off = remuda_protocol::creator_permission::is_control_off(creator);
            if own_host && control_off {
                (WakeCheckAdmission::OwnHostBypass, None)
            } else {
                let mut why = Vec::new();
                if !own_host {
                    why.push("another-host");
                }
                if !control_off {
                    why.push("creator-keeps-permission-control-on");
                }
                // The exact check the owner approves: a changed command,
                // host or timeout is a new request (a new add, never an
                // edit).
                let request = json!({
                    "operation": "wake.check",
                    "lineageId": caller.lineage_id,
                    "name": checked.name,
                    "hostId": host_id,
                    "command": check.command,
                    "timeoutSecs": check.timeout_secs,
                    "why": why,
                });
                crate::agent_scope::require_approval(state, headers, &caller.device, request)
                    .await?;
                let approval_id = headers
                    .get("x-remuda-approval-id")
                    .and_then(|raw| raw.to_str().ok())
                    .map(str::to_owned);
                (WakeCheckAdmission::Approved, approval_id)
            }
        }
    };
    tracing::info!(
        category = "wake",
        outcome = "check-admitted",
        admission = admission.as_str()
    );
    Ok(Some(WakeCheck {
        command: check.command.clone(),
        host_id,
        timeout_secs: check.timeout_secs,
        admission,
        approval_id,
    }))
}

/// 409 for a name the lineage already holds with a different wake-up (a
/// changed check included: changing it needs a new add, never an edit).
fn name_taken() -> WakeError {
    WakeError::Conflict {
        reason: "wake-name-taken",
        message: "the lineage already has a different wake-up under this name".into(),
        hint: "remove it first (remuda wake rm <name>) or add under another name; a check \
               is changed by a new add, never an edit",
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
    let admitted = match &checked.check {
        Some(check) => admit_check(&state, &headers, &caller, &checked, check).await?,
        None => None,
    };
    let added = state
        .store
        .run_named("wake_add", {
            let caller = caller.clone();
            move |conn| add(conn, &caller, &checked, admitted.as_ref())
        })
        .await
        .map_err(map_store)?;
    let (status, wake_up, created) = match added {
        Added::Created(wake) => (StatusCode::CREATED, wake, true),
        Added::Existing(wake) => (StatusCode::OK, wake, false),
        Added::NameTaken => return Err(name_taken()),
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
        Added::ClaimedPane => {
            return Err(crate::herdr_claims::takes_no_hub_input("wake-ups").into());
        }
        Added::OutOfScope => return Err(HubError::out_of_scope().into()),
        Added::NeedsAdmission => {
            return Err(WakeError::Conflict {
                reason: "wake-changed",
                message: "the wake-up this add repeated was removed meanwhile".into(),
                hint: "add it again; its check is admitted afresh",
            });
        }
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
    /// A run waits for its check.
    pub(crate) check_due: bool,
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
    check_json: Option<String>,
}

const TICK_COLUMNS: &str = "wake_id, lineage_id, name, text, trigger_kind, trigger_json, \
     every_secs, mode, notify, next_fire_at, missed_count, when_cursor, creator_origin, \
     check_json";

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
        check_json: row.get(13)?,
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
    // §11.2: a run of a wake-up with a check waits for that check; a stored
    // check this build cannot read never lets a run through unchecked.
    let check: Option<WakeCheck> = match wake.check_json.as_deref() {
        None => None,
        Some(raw) => match serde_json::from_str(raw) {
            Ok(check) => Some(check),
            Err(_) => {
                crate::inbox::report_once("wake-check-unreadable");
                return Ok(());
            }
        },
    };
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
             fired_at, state, missed_before, check_json, check_state, check_host_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'queued', ?10, ?11, ?12, ?13)",
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
            check.as_ref().map(|check| json!({
                "command": check.command,
                "hostId": check.host_id,
                "timeoutSecs": check.timeout_secs,
            })
            .to_string()),
            check.as_ref().map(|_| WakeCheckState::Pending.as_str()),
            check.as_ref().map(|check| check.host_id.as_str()),
        ],
    )?;
    // A run without a check is a real run from the moment it fires; one with
    // a check becomes one only when its check passes (§11.2: a skipped run
    // never refreshes the last real run).
    conn.execute(
        "UPDATE wake_ups SET missed_count = 0, last_fired_at = ?2, updated_at = ?2,
                last_run_at = CASE WHEN ?3 THEN ?2 ELSE last_run_at END
          WHERE wake_id = ?1",
        params![wake.wake_id, now, check.is_none()],
    )?;
    outcome.fired += 1;
    if check.is_some() {
        outcome.check_due = true;
    } else {
        deliver(
            conn,
            &run_id,
            &wake.lineage_id,
            &wake.mode,
            detail,
            now,
            outcome,
        )?;
    }
    tracing::info!(
        category = "wake",
        outcome = "fired",
        trigger = wake.trigger_kind.as_str(),
        mode = wake.mode.as_str(),
        check = check.is_some(),
        missed
    );
    Ok(())
}

/// Hand a run that may happen to its mode: a fresh run waits for its child;
/// a persistent run becomes its `wake-due` fact.
fn deliver(
    conn: &Connection,
    run_id: &str,
    lineage_id: &str,
    mode: &str,
    detail: Value,
    now: &str,
    outcome: &mut TickOutcome,
) -> Result<(), StoreError> {
    if mode == WakeMode::Fresh.as_str() {
        outcome.fresh_due = true;
        return Ok(());
    }
    // §11.1: delivered through the inbox, so it waits for the idle point
    // like any fact. Each run is its own fact (its id is the key and the
    // cap subject), so the occurrence cap never folds two runs together.
    let recorded = crate::inbox::record(
        conn,
        lineage_id,
        crate::inbox::Fact {
            kind: InboxFactKind::WakeDue,
            subject_instance_id: None,
            key_subject: run_id.to_owned(),
            cap_subject: run_id.to_owned(),
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
        _ => queue_outcome(conn, run_id, "failed", Some(failure::NOT_RECEIVED), now)?,
    }
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
/// fresh child never count, so a wake-up cannot keep re-firing itself. A
/// `when` wake-up whose lineage is gone turns `done`.
fn due_conditions(
    conn: &Connection,
    now_s: &str,
    outcome: &mut TickOutcome,
) -> Result<(), StoreError> {
    // A lineage that no longer exists records no facts, so its `when`
    // wake-ups could never fire again: they turn `done`, as a scheduled
    // wake-up does when it comes due.
    let gone = conn.execute(
        "UPDATE wake_ups SET state = 'done', updated_at = ?1
          WHERE state = 'active' AND trigger_kind = 'when'
            AND NOT EXISTS (SELECT 1 FROM lineages l WHERE l.lineage_id = wake_ups.lineage_id)
            AND NOT EXISTS (SELECT 1 FROM instances i WHERE i.id = wake_ups.lineage_id)",
        params![now_s],
    )?;
    if gone > 0 {
        tracing::info!(category = "wake", outcome = "lineage-gone", wake_ups = gone);
    }
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
    let reports = drain_check_reports(conn, now);
    drain_outcomes(conn, now);
    let now_s = rfc3339(now);
    let mut outcome = TickOutcome::default();
    let tx = immediate_tx(conn)?;
    due_scheduled(&tx, now, &now_s, &mut outcome)?;
    due_conditions(&tx, &now_s, &mut outcome)?;
    let (oldest, fresh, check): (Option<String>, bool, bool) = tx.query_row(
        "SELECT (SELECT MIN(fired_at) FROM wake_runs WHERE state = 'queued'),
                EXISTS (SELECT 1 FROM wake_runs WHERE state = 'queued' AND mode = 'fresh'
                           AND (check_state IS NULL OR check_state = 'passed')),
                EXISTS (SELECT 1 FROM wake_runs WHERE state = 'queued'
                           AND check_state = 'pending')",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    tx.commit()?;
    outcome.oldest_waiting = oldest.as_deref().and_then(crate::inbox::parse_at);
    outcome.fresh_due |= fresh || reports.fresh;
    outcome.check_due |= check;
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
            if outcome.check_due {
                drive_checks(state).await;
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

/// Queue the result of `run_id` in the caller's transaction: `success`,
/// `failed` (with its kind) or `skipped` (its check exited 2).
fn queue_outcome(
    conn: &Connection,
    run_id: &str,
    outcome: &str,
    kind: Option<&str>,
    at: &str,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO wake_outcomes (run_id, outcome, failure_kind, observed_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![run_id, outcome, kind, at],
    )?;
    Ok(())
}

/// One queued result.
#[derive(Debug, Clone)]
struct QueuedOutcome {
    id: i64,
    run_id: String,
    outcome: String,
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
                outcome: row.get(2)?,
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
    let (state, kind) = match row.outcome.as_str() {
        "success" => ("success", None),
        "skipped" => ("skipped", None),
        _ => (
            "failed",
            Some(row.kind.as_deref().unwrap_or(failure::DELIVERY)),
        ),
    };
    conn.execute(
        "UPDATE wake_runs SET state = ?2, ended_at = ?3, failure_kind = ?4
          WHERE run_id = ?1 AND state IN ('queued', 'running')",
        params![row.run_id, state, row.at, kind],
    )?;
    // A skipped run did not happen: it notifies nothing under any policy
    // and neither sets nor clears a failure latch.
    let notified = if notify && state != "skipped" {
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
        failure::CHECK_EXIT => "its check exited with an error",
        failure::CHECK_TIMEOUT => "its check ran into its timeout and was killed",
        failure::CHECK_SYNTAX => "its check is not valid shell; nothing ran",
        failure::CHECK_UNAVAILABLE => "its check could not start on its Node",
        failure::CHECK_NO_WORKSPACE => "the lineage has no workspace on its check's host",
        failure::CHECK_CWD_UNUSABLE => {
            "its check's directory in the lineage's workspace is not usable"
        }
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

/// At open, before any job: apply the check reports and then the results the
/// previous process left queued, then mark every run still `running` that
/// has neither queued as `interrupted`, its check too (a queued result or
/// report is evidence; it is applied on a later pass). An interrupted run is
/// never replayed; its wake-up's next occurrence is computed from the
/// schedule as usual.
pub(crate) fn boot(conn: &mut Connection) {
    drain_check_reports(conn, OffsetDateTime::now_utc());
    drain_outcomes(conn, OffsetDateTime::now_utc());
    // A check in flight is interrupted with its run: its Node's answer, if
    // one ever comes, goes to a link that no longer exists.
    match conn.execute(
        "UPDATE wake_runs SET state = 'interrupted', ended_at = ?1,
                check_state = CASE WHEN check_state = 'running' THEN 'interrupted'
                                   ELSE check_state END
          WHERE state = 'running'
            AND run_id NOT IN (SELECT run_id FROM wake_outcomes)
            AND run_id NOT IN (SELECT run_id FROM wake_check_reports)",
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

// ── checks (§11.2) ─────────────────────────────────────────────────────────

/// A run's check as its run row keeps it (the wake-up may be removed while
/// the run waits; the run still checks what it was fired with).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunCheckSpec {
    command: String,
    host_id: String,
    timeout_secs: i64,
}

/// A check claimed for its Node.
#[derive(Debug, Clone)]
struct CheckClaim {
    run_id: String,
    wake_id: String,
    host_id: String,
    timeout_secs: u64,
    params: remuda_protocol::WakeCheckParams,
}

/// What came back from the Node.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "result", rename_all = "kebab-case")]
enum CheckReport {
    /// The Node ran it (or refused to start it) and said how it went.
    Done(remuda_protocol::WakeCheckResult),
    /// The Node answered the call with an error: it did not run.
    Refused,
    /// The host was not connected when the call was made: it provably never
    /// reached the Node.
    NotSent,
    /// The link went away (or stayed silent) while the check may have been
    /// running: whether it ran is unknown.
    Lost,
}

/// Where a check runs on its host: the lineage's workspace there.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CheckPlace {
    pub(crate) workspace_id: Option<String>,
    pub(crate) cwd: Option<String>,
    /// The lineage's chapter on that host: its Node resolves the instance's
    /// own workspace when the Hub has none on record.
    pub(crate) instance_id: Option<String>,
}

/// The lineage's workspace on `host_id`, or `None` when it has none there
/// (r2: a check then never runs, and never in a workspace its Node picks).
///
/// * The current chapter lives on that host: its workspace and cwd (the
///   workspace the Hub recorded, else its spec's, else its project's member
///   there), and the chapter itself, so its Node can resolve the workspace
///   the instance really runs in.
/// * Otherwise the workspace its project has on that host (its primary
///   member first).
pub(crate) fn check_place(
    conn: &Connection,
    lineage_id: &str,
    host_id: &str,
) -> Result<Option<CheckPlace>, StoreError> {
    #[allow(clippy::type_complexity)]
    let chapter: Option<(String, String, Option<String>, String, Option<String>)> = conn
        .query_row(
            "SELECT id, host_id, workspace_id, spec_json, scope_json FROM instances
              WHERE id = COALESCE((SELECT current_instance_id FROM lineages
                                    WHERE lineage_id = ?1), ?1)",
            params![lineage_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    let Some((chapter_id, chapter_host, workspace_id, spec, scope)) = chapter else {
        return Ok(None);
    };
    let spec: Value = serde_json::from_str(&spec).unwrap_or(Value::Null);
    let text = |field: &str| {
        spec.get(field)
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
    };
    let project = scope
        .and_then(|raw| serde_json::from_str::<remuda_protocol::InstanceScope>(&raw).ok())
        .and_then(|scope| scope.single_project_id().map(|id| id.as_id().to_string()))
        .or_else(|| text("projectId"));
    let member = match &project {
        Some(project) => conn
            .query_row(
                "SELECT json_extract(m.value, '$.workspaceId')
                   FROM projects p, json_each(p.doc_json, '$.members') m
                  WHERE p.id = ?1 AND json_extract(m.value, '$.hostId') = ?2
                  ORDER BY json_extract(m.value, '$.role') = 'primary' DESC
                  LIMIT 1",
                params![project, host_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten(),
        None => None,
    };
    if chapter_host == host_id {
        return Ok(Some(CheckPlace {
            workspace_id: workspace_id.or_else(|| text("workspaceId")).or(member),
            cwd: text("cwd"),
            instance_id: Some(chapter_id),
        }));
    }
    Ok(member.map(|workspace| CheckPlace {
        workspace_id: Some(workspace),
        cwd: None,
        instance_id: None,
    }))
}

/// Claim the check of `run_id` when its host is online: the run turns
/// `running` (and its check `running`) in this transaction, before the
/// Node is asked, so a Hub that stops mid-check finds it running at boot
/// and interrupts it rather than asking again. `Ok(None)`: not pending, or
/// held for a later tick (its host is offline). A stored check this build
/// cannot read fails the run here.
fn claim_check(conn: &mut Connection, run_id: &str) -> Result<Option<CheckClaim>, StoreError> {
    let tx = immediate_tx(conn)?;
    #[allow(clippy::type_complexity)]
    let run: Option<(String, String, String, String, String, Option<String>)> = tx
        .query_row(
            "SELECT wake_id, lineage_id, name, scheduled_at, fired_at, check_json
               FROM wake_runs
              WHERE run_id = ?1 AND state = 'queued' AND check_state = 'pending'
                AND (check_next_at IS NULL OR check_next_at <= ?2)",
            params![run_id, now_rfc3339()],
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
    let Some((wake_id, lineage_id, name, scheduled_at, fired_at, spec)) = run else {
        return Ok(None);
    };
    let now = now_rfc3339();
    let spec = spec
        .and_then(|raw| serde_json::from_str::<RunCheckSpec>(&raw).ok())
        .filter(|spec| {
            (WAKE_CHECK_TIMEOUT_MIN_SECS..=WAKE_CHECK_TIMEOUT_MAX_SECS).contains(&spec.timeout_secs)
        });
    let Some(spec) = spec else {
        crate::inbox::report_once("wake-check-unreadable");
        tx.execute(
            "UPDATE wake_runs SET check_state = 'failed', check_summary = 'unreadable'
              WHERE run_id = ?1",
            params![run_id],
        )?;
        queue_outcome(
            &tx,
            run_id,
            "failed",
            Some(failure::CHECK_UNAVAILABLE),
            &now,
        )?;
        tx.commit()?;
        return Ok(None);
    };
    let online: bool = tx
        .query_row(
            "SELECT state = 'online' FROM hosts WHERE id = ?1",
            params![spec.host_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(false);
    if !online {
        return Ok(None);
    }
    let Some(place) = check_place(&tx, &lineage_id, &spec.host_id)? else {
        // r2: the lineage no longer has a workspace on that host (the
        // chapter moved, the project member was removed): the run fails, and
        // nothing is asked of the Node.
        tx.execute(
            "UPDATE wake_runs SET state = 'running', check_state = 'running', started_at = ?2
              WHERE run_id = ?1",
            params![run_id, now],
        )?;
        let report = CheckReport::Done(remuda_protocol::WakeCheckResult {
            run_id: run_id.to_owned(),
            outcome: remuda_protocol::WakeCheckOutcome::NoWorkspace,
            exit_code: None,
            signal: None,
            stdout: String::new(),
            stdout_truncated: false,
            summary: "the lineage has no workspace on that host; nothing ran".into(),
            containment: None,
            containment_incomplete: false,
        });
        apply_report(&tx, run_id, &report)?;
        tx.commit()?;
        return Ok(None);
    };
    let last_run_at: Option<String> = tx
        .query_row(
            "SELECT last_run_at FROM wake_ups WHERE wake_id = ?1",
            params![wake_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let claimed = tx.execute(
        "UPDATE wake_runs SET state = 'running', check_state = 'running', started_at = ?2
          WHERE run_id = ?1 AND state = 'queued' AND check_state = 'pending'",
        params![run_id, now],
    )?;
    if claimed == 0 {
        return Ok(None);
    }
    tx.commit()?;
    // What a check may read about its run (§11.2: "time since the last real
    // run" is the reason a skip must not refresh `lastRunAt`).
    let mut env = std::collections::BTreeMap::new();
    for (key, value) in [
        ("REMUDA_WAKE_ID", Some(wake_id.clone())),
        ("REMUDA_WAKE_NAME", Some(name)),
        ("REMUDA_WAKE_RUN_ID", Some(run_id.to_owned())),
        ("REMUDA_WAKE_LINEAGE_ID", Some(lineage_id)),
        ("REMUDA_WAKE_SCHEDULED_AT", Some(scheduled_at)),
        ("REMUDA_WAKE_FIRED_AT", Some(fired_at)),
        ("REMUDA_WAKE_LAST_RUN_AT", last_run_at),
    ] {
        if let Some(value) = value {
            env.insert(key.to_owned(), value);
        }
    }
    let timeout_secs = u64::try_from(spec.timeout_secs).unwrap_or(1);
    Ok(Some(CheckClaim {
        run_id: run_id.to_owned(),
        wake_id: wake_id.clone(),
        host_id: spec.host_id,
        timeout_secs,
        params: remuda_protocol::WakeCheckParams {
            run_id: run_id.to_owned(),
            command: spec.command,
            timeout_secs,
            workspace_id: place.workspace_id,
            cwd: place.cwd,
            instance_id: place.instance_id,
            env,
        },
    }))
}

/// Ask the Node to run every check that is due and whose host is online.
/// Each check is its own task, so a long check never holds the tick; the
/// durable claim keeps two drives from asking twice.
pub(crate) async fn drive_checks(state: &AppState) {
    let runs = state
        .store
        .read("wake_checks_due", |conn| {
            Ok(conn
                .prepare_cached(
                    "SELECT run_id FROM wake_runs WHERE state = 'queued' AND check_state = 'pending'
                       AND (check_next_at IS NULL OR check_next_at <= ?2)
                      ORDER BY fired_at, run_id LIMIT ?1",
                )?
                .query_map(params![CHECKS_PER_DRIVE, now_rfc3339()], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await;
    let runs = match runs {
        Ok(runs) => runs,
        Err(err) => {
            tracing::error!(
                category = "wake",
                code = crate::inbox::error_code(&err),
                "due checks could not be read"
            );
            return;
        }
    };
    for run_id in runs {
        let claimed = state
            .store
            .run_named("wake_check_claim", move |conn| claim_check(conn, &run_id))
            .await;
        match claimed {
            Ok(Some(claim)) => {
                state
                    .wake
                    .checks_in_flight
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let state = state.clone();
                tokio::spawn(async move {
                    run_check(&state, claim).await;
                    state
                        .wake
                        .checks_in_flight
                        .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    state.wake.check_settled.notify_waiters();
                });
            }
            Ok(None) => {}
            Err(err) => tracing::error!(
                category = "wake",
                code = crate::inbox::error_code(&err),
                "check claim failed"
            ),
        }
    }
}

/// One claimed check: the Node call, then its settlement.
async fn run_check(state: &AppState, claim: CheckClaim) {
    let params = match serde_json::to_value(&claim.params) {
        Ok(params) => params,
        Err(_) => return,
    };
    #[cfg(any(test, feature = "test-faults"))]
    let busy = test_seams::take_check_node_busy(&claim.run_id)
        || test_seams::take_check_node_busy(&claim.wake_id);
    #[cfg(not(any(test, feature = "test-faults")))]
    let busy = false;
    let answer = if busy {
        Err(HubError::NodeBusy {
            retry_after_ms: 1000,
        })
    } else {
        state
            .nodes
            .call(
                &claim.host_id,
                remuda_protocol::METHOD_WAKE_CHECK,
                params,
                check_rpc_wait(claim.timeout_secs),
            )
            .await
    };
    let report = match answer {
        Ok(Some(frame)) => match frame.get("result") {
            Some(result) => serde_json::from_value(result.clone())
                .map_or(CheckReport::Refused, CheckReport::Done),
            None => CheckReport::Refused,
        },
        // Not connected, or the link refused the frame before queueing it
        // (NODE_BUSY: too many calls in flight): the Node never saw it.
        Ok(None) | Err(HubError::NodeBusy { .. }) => CheckReport::NotSent,
        // The frame was queued and the reply never came (the link went away,
        // or the Node stayed silent past what it can take): it may have run.
        Err(_) => CheckReport::Lost,
    };
    #[cfg(any(test, feature = "test-faults"))]
    if state
        .wake
        .drop_check_answers
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        return;
    }
    // §11.1: only the write is retried, never the run. The report is queued
    // in a small write of its own (retried here while it fails), then applied
    // by the drain every writer job runs, which retries its own failures.
    let run_id = claim.run_id;
    let report = match serde_json::to_string(&report) {
        Ok(report) => report,
        Err(_) => return,
    };
    let mut attempts = 0_i64;
    loop {
        attempts += 1;
        let queued = state
            .store
            .run_named("wake_check_report", {
                let run_id = run_id.clone();
                let report = report.clone();
                let wake_id = claim.wake_id.clone();
                // Applied in the same job, so this task learns what it left
                // to do; a failed apply stays queued for the next drain
                // (every writer job, every tick), whose fresh children and
                // retries the tick picks up.
                move |conn| {
                    queue_report(conn, &run_id, &wake_id, &report)?;
                    Ok(drain_check_reports(conn, OffsetDateTime::now_utc()))
                }
            })
            .await;
        match queued {
            Ok(drained) => {
                if let Some(secs) = drained.retry_after {
                    tokio::spawn(redrive_checks(state.clone(), secs));
                }
                if drained.fresh {
                    drive_fresh(state).await;
                }
                return;
            }
            Err(err) if attempts < MAX_OUTCOME_ATTEMPTS => {
                tracing::warn!(
                    category = "wake",
                    code = crate::inbox::error_code(&err),
                    attempts,
                    "a check report could not be queued; it is retried"
                );
                let secs = check_retry_secs(attempts).min(OUTCOME_RETRY_MAX_SECS);
                tokio::time::sleep(std::time::Duration::from_secs(
                    u64::try_from(secs).unwrap_or(1),
                ))
                .await;
            }
            Err(err) => {
                // The run stays running; a Hub restart interrupts it.
                tracing::error!(
                    category = "wake",
                    code = crate::inbox::error_code(&err),
                    "a check report could not be queued; given up"
                );
                return;
            }
        }
    }
}

/// Queue a Node's report on `run_id`'s check, in its own write.
fn queue_report(
    conn: &mut Connection,
    run_id: &str,
    wake_id: &str,
    report: &str,
) -> Result<(), StoreError> {
    #[cfg(any(test, feature = "test-faults"))]
    if test_seams::take_check_report_failure(run_id)
        || test_seams::take_check_report_failure(wake_id)
    {
        return Err(StoreError::FaultInjected("wake check report".into()));
    }
    conn.execute(
        "INSERT INTO wake_check_reports (run_id, report_json, observed_at) VALUES (?1, ?2, ?3)",
        params![run_id, report, now_rfc3339()],
    )?;
    Ok(())
}

/// What a drain of queued check reports left to do.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Drained {
    /// A fresh run may now create its child.
    pub(crate) fresh: bool,
    /// A check its Node never received is asked again after this many seconds.
    pub(crate) retry_after: Option<u64>,
}

/// Apply the queued check reports due at `now` (§11.1: only the write is
/// retried, never the run). Runs after every writer job, on every tick and at
/// open. A report whose apply fails stays queued and is tried again after
/// 1 s, doubling, at most 60 s; after [`MAX_OUTCOME_ATTEMPTS`] failed tries
/// the run's own state is written without its delivery, fact or notice
/// ([`apply_report_state_only`]), so a run always ends.
pub(crate) fn drain_check_reports(conn: &mut Connection, now: OffsetDateTime) -> Drained {
    let mut drained = Drained::default();
    let now_s = rfc3339(now);
    let pending = conn
        .prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM wake_check_reports
                             WHERE next_attempt_at IS NULL OR next_attempt_at <= ?1)",
        )
        .and_then(|mut stmt| stmt.query_row(params![now_s], |row| row.get::<_, bool>(0)));
    if !matches!(pending, Ok(true)) {
        return drained;
    }
    let result = (|| -> Result<(), StoreError> {
        let tx = immediate_tx(conn)?;
        let rows: Vec<(i64, String, String, i64)> = tx
            .prepare_cached(
                "SELECT id, run_id, report_json, attempts FROM wake_check_reports
                  WHERE next_attempt_at IS NULL OR next_attempt_at <= ?1
                  ORDER BY id LIMIT ?2",
            )?
            .query_map(params![now_s, OUTCOME_BATCH], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        for (id, run_id, raw, attempts) in rows {
            let report: CheckReport = match serde_json::from_str(&raw) {
                Ok(report) => report,
                // A report this build cannot read ends its run as unavailable.
                Err(_) => CheckReport::Refused,
            };
            let applied =
                in_savepoint_with(&tx, || apply_report(&tx, &run_id, &report)).or_else(|err| {
                    if attempts + 1 >= MAX_OUTCOME_ATTEMPTS {
                        in_savepoint_with(&tx, || apply_report_state_only(&tx, &run_id, &report))
                            .map_err(|_| err)
                    } else {
                        Err(err)
                    }
                });
            match applied {
                Ok(settled) => {
                    tx.execute("DELETE FROM wake_check_reports WHERE id = ?1", params![id])?;
                    match settled {
                        Settled::Fresh => drained.fresh = true,
                        Settled::RetryAfter(secs) => {
                            drained.retry_after =
                                Some(drained.retry_after.map_or(secs, |held| held.min(secs)));
                        }
                        Settled::Done => {}
                    }
                }
                Err(err) => {
                    tx.execute(
                        "UPDATE wake_check_reports SET attempts = attempts + 1, next_attempt_at = ?2
                          WHERE id = ?1",
                        params![id, retry_at(now, attempts.saturating_add(1))],
                    )?;
                    tracing::warn!(
                        category = "wake",
                        code = crate::inbox::error_code(&err),
                        attempts = attempts + 1,
                        "a check settle write failed; it is retried"
                    );
                }
            }
        }
        tx.commit()?;
        Ok(())
    })();
    if let Err(err) = result {
        tracing::error!(
            category = "wake",
            code = crate::inbox::error_code(&err),
            "check report drain failed"
        );
    }
    drained
}

/// [`in_savepoint`] for work that returns a value.
fn in_savepoint_with<T>(
    conn: &Connection,
    work: impl FnOnce() -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    conn.execute_batch("SAVEPOINT wake_check_settle")?;
    match work() {
        Ok(value) => {
            conn.execute_batch("RELEASE wake_check_settle")?;
            Ok(value)
        }
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK TO wake_check_settle; RELEASE wake_check_settle");
            Err(err)
        }
    }
}

/// The containment a Node reported, as the run stores it.
fn report_containment(report: &CheckReport) -> Option<String> {
    match report {
        CheckReport::Done(result) => result
            .containment
            .and_then(|label| serde_json::to_value(label).ok())
            .and_then(|label| label.as_str().map(str::to_owned)),
        _ => None,
    }
}

/// The last resort of a report whose full apply kept failing: the run and its
/// check end in the state the report names, with nothing else written (no
/// delivery, no fact, no notice). A check that passed cannot run any more:
/// its run fails `not-delivered`. A report that never reached the Node, or a
/// lost link, interrupts the run.
fn apply_report_state_only(
    conn: &Connection,
    run_id: &str,
    report: &CheckReport,
) -> Result<Settled, StoreError> {
    let (state, check_state, kind, exit) = match report {
        CheckReport::NotSent | CheckReport::Lost => ("interrupted", "interrupted", None, None),
        CheckReport::Refused => ("failed", "failed", Some(failure::CHECK_UNAVAILABLE), None),
        CheckReport::Done(result) => {
            let exit = (result.outcome == remuda_protocol::WakeCheckOutcome::Exited)
                .then_some(result.exit_code)
                .flatten();
            match (result.outcome, exit) {
                (remuda_protocol::WakeCheckOutcome::Exited, Some(0)) => {
                    ("failed", "passed", Some(failure::NOT_DELIVERED), exit)
                }
                (remuda_protocol::WakeCheckOutcome::Exited, Some(2)) => {
                    ("skipped", "skipped", None, exit)
                }
                (remuda_protocol::WakeCheckOutcome::Exited, _) => {
                    ("failed", "failed", Some(failure::CHECK_EXIT), exit)
                }
                (remuda_protocol::WakeCheckOutcome::TimedOut, _) => {
                    ("failed", "failed", Some(failure::CHECK_TIMEOUT), None)
                }
                (remuda_protocol::WakeCheckOutcome::Syntax, _) => {
                    ("failed", "failed", Some(failure::CHECK_SYNTAX), None)
                }
                (remuda_protocol::WakeCheckOutcome::Unavailable, _) => {
                    ("failed", "failed", Some(failure::CHECK_UNAVAILABLE), None)
                }
                (remuda_protocol::WakeCheckOutcome::NoWorkspace, _) => {
                    ("failed", "failed", Some(failure::CHECK_NO_WORKSPACE), None)
                }
                (remuda_protocol::WakeCheckOutcome::CwdUnusable, _) => {
                    ("failed", "failed", Some(failure::CHECK_CWD_UNUSABLE), None)
                }
            }
        }
    };
    conn.execute(
        "UPDATE wake_runs SET state = ?2, check_state = ?3, failure_kind = ?4, check_exit = ?5,
                ended_at = ?6, check_containment = ?7, check_containment_incomplete = ?8
          WHERE run_id = ?1 AND state = 'running' AND check_state = 'running'",
        params![
            run_id,
            state,
            check_state,
            kind,
            exit,
            now_rfc3339(),
            report_containment(report),
            matches!(report, CheckReport::Done(result) if result.containment_incomplete),
        ],
    )?;
    tracing::warn!(
        category = "wake",
        outcome = "check-settled-state-only",
        "a check report was written without its delivery, fact or notice"
    );
    Ok(Settled::Done)
}

/// Drive the due checks again after `secs` (a check its Node never
/// received). Boxed: it is spawned from inside a check the drive started.
fn redrive_checks(
    state: AppState,
    secs: u64,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async move {
        tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
        drive_checks(&state).await;
    })
}

/// What a check's settlement leaves to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Settled {
    /// Nothing.
    Done,
    /// A fresh run may now create its child.
    Fresh,
    /// The call never reached the Node: ask again after this many seconds.
    RetryAfter(u64),
}

/// Apply what a check did (§11.2), in the drain's savepoint.
///
/// * Exit 0: the run happens. It is the wake-up's last real run, and its
///   bounded stdout rides the run's detail as its summary (quoted as
///   untrusted data in the envelope or the fresh brief).
/// * Exit 2: `skipped`. No model turn, no notice, and the last real run is
///   left as it was.
/// * Any other exit, a signal, the timeout, a syntax error or a check that
///   could not start: `failed`, with a `wake-failed` fact to the lineage; the
///   `notify` declaration applies with its repeat-failure latch.
/// * The call never reached the Node (not connected, or NODE_BUSY before the
///   frame was queued): the check waits again, and its next attempt waits
///   1 s, doubling, at most 60 s.
/// * The link went away mid-check: `interrupted`, never asked again (the
///   rule a Hub restart applies at boot).
fn apply_report(
    tx: &Connection,
    run_id: &str,
    report: &CheckReport,
) -> Result<Settled, StoreError> {
    #[allow(clippy::type_complexity)]
    let run: Option<(
        String,
        String,
        String,
        String,
        String,
        String,
        Option<String>,
    )> = tx
        .query_row(
            "SELECT wake_id, lineage_id, name, mode, fired_at, detail_json, check_host_id
               FROM wake_runs
              WHERE run_id = ?1 AND state = 'running' AND check_state = 'running'",
            params![run_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .optional()?;
    let Some((wake_id, lineage_id, name, mode, fired_at, detail, host_id)) = run else {
        return Ok(Settled::Done);
    };
    #[cfg(any(test, feature = "test-faults"))]
    if test_seams::take_check_settle_failure(run_id)
        || test_seams::take_check_settle_failure(&wake_id)
    {
        return Err(StoreError::FaultInjected("wake check settle".into()));
    }
    let host_id = host_id.unwrap_or_default();
    let now = now_rfc3339();
    let result = match report {
        CheckReport::NotSent => {
            let attempts: i64 = tx.query_row(
                "SELECT check_attempts + 1 FROM wake_runs WHERE run_id = ?1",
                params![run_id],
                |row| row.get(0),
            )?;
            let secs = check_retry_secs(attempts);
            let next = OffsetDateTime::now_utc().saturating_add(time::Duration::seconds(secs));
            tx.execute(
                "UPDATE wake_runs SET state = 'queued', check_state = 'pending', started_at = NULL,
                        check_attempts = ?2, check_next_at = ?3
                  WHERE run_id = ?1",
                params![run_id, attempts, rfc3339(next)],
            )?;
            tracing::info!(category = "wake", outcome = "check-not-sent", attempts);
            return Ok(Settled::RetryAfter(u64::try_from(secs).unwrap_or(1)));
        }
        CheckReport::Lost => {
            tx.execute(
                "UPDATE wake_runs SET state = 'interrupted', check_state = 'interrupted',
                        ended_at = ?2, check_summary = 'the link to its Node was lost'
                  WHERE run_id = ?1",
                params![run_id, now],
            )?;
            tracing::info!(category = "wake", outcome = "check-interrupted");
            return Ok(Settled::Done);
        }
        CheckReport::Refused => None,
        CheckReport::Done(result) => Some(result),
    };
    let exit = result.and_then(|result| {
        (result.outcome == remuda_protocol::WakeCheckOutcome::Exited)
            .then_some(result.exit_code)
            .flatten()
    });
    let (summary, output, truncated) = match result {
        Some(result) => (
            result.summary.clone(),
            Some(result.stdout.clone()),
            result.stdout_truncated,
        ),
        None => ("its Node refused the call".to_owned(), None, false),
    };
    // A Node's report is bounded here too: the Hub keeps no more than the
    // protocol allows, whatever arrived.
    let output = output.map(|text| {
        let mut end = text.len().min(remuda_protocol::WAKE_CHECK_STDOUT_MAX_BYTES);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text[..end].to_owned()
    });
    let summary: String = summary.chars().take(200).collect();
    let (check_state, failure_kind) = match (result.map(|result| result.outcome), exit) {
        (Some(remuda_protocol::WakeCheckOutcome::Exited), Some(0)) => ("passed", None),
        (Some(remuda_protocol::WakeCheckOutcome::Exited), Some(2)) => ("skipped", None),
        (Some(remuda_protocol::WakeCheckOutcome::Exited), _) => {
            ("failed", Some(failure::CHECK_EXIT))
        }
        (Some(remuda_protocol::WakeCheckOutcome::TimedOut), _) => {
            ("failed", Some(failure::CHECK_TIMEOUT))
        }
        (Some(remuda_protocol::WakeCheckOutcome::Syntax), _) => {
            ("failed", Some(failure::CHECK_SYNTAX))
        }
        (Some(remuda_protocol::WakeCheckOutcome::NoWorkspace), _) => {
            ("failed", Some(failure::CHECK_NO_WORKSPACE))
        }
        (Some(remuda_protocol::WakeCheckOutcome::CwdUnusable), _) => {
            ("failed", Some(failure::CHECK_CWD_UNUSABLE))
        }
        (Some(remuda_protocol::WakeCheckOutcome::Unavailable), _) | (None, _) => {
            ("failed", Some(failure::CHECK_UNAVAILABLE))
        }
    };
    let containment = report_containment(report);
    let incomplete = result.is_some_and(|result| result.containment_incomplete);
    tx.execute(
        "UPDATE wake_runs SET check_state = ?2, check_exit = ?3, check_summary = ?4,
                check_output = ?5, check_output_truncated = ?6, check_containment = ?7,
                check_containment_incomplete = ?8
          WHERE run_id = ?1",
        params![
            run_id,
            check_state,
            exit,
            summary,
            output,
            truncated,
            containment,
            incomplete
        ],
    )?;
    let mut fresh_due = false;
    match check_state {
        "passed" => {
            let mut detail: Value = serde_json::from_str(&detail).unwrap_or_else(|_| json!({}));
            detail["check"] = json!({
                "hostId": host_id,
                "exit": 0,
                "output": output.clone().unwrap_or_default(),
                "outputTruncated": truncated,
            });
            tx.execute(
                "UPDATE wake_runs SET state = 'queued', detail_json = ?2 WHERE run_id = ?1",
                params![run_id, detail.to_string()],
            )?;
            tx.execute(
                "UPDATE wake_ups SET last_run_at = ?2 WHERE wake_id = ?1",
                params![wake_id, fired_at],
            )?;
            let mut outcome = TickOutcome::default();
            deliver(tx, run_id, &lineage_id, &mode, detail, &now, &mut outcome)?;
            fresh_due = outcome.fresh_due;
        }
        "skipped" => queue_outcome(tx, run_id, "skipped", None, &now)?,
        _ => {
            let kind = failure_kind.unwrap_or(failure::CHECK_UNAVAILABLE);
            queue_outcome(tx, run_id, "failed", Some(kind), &now)?;
            // The lineage hears that its check failed, at its idle point
            // like any fact; the check's output is not carried.
            crate::inbox::record(
                tx,
                &lineage_id,
                crate::inbox::Fact {
                    kind: InboxFactKind::WakeFailed,
                    subject_instance_id: None,
                    key_subject: run_id.to_owned(),
                    cap_subject: wake_id.clone(),
                    run: 0,
                    run_in_key: false,
                    entered_at: now.clone(),
                    project_id: None,
                    payload: json!({
                        "wakeId": wake_id,
                        "runId": run_id,
                        "name": name,
                        "hostId": host_id,
                        "failureKind": kind,
                        "checkExit": exit,
                        "summary": summary,
                    }),
                },
            )?;
        }
    }
    tracing::info!(
        category = "wake",
        outcome = match check_state {
            "passed" => "check-passed",
            "skipped" => "check-skipped",
            _ => "check-failed",
        },
        failure = failure_kind.unwrap_or(""),
        exit_code = exit,
        "wake check settled"
    );
    Ok(if fresh_due {
        Settled::Fresh
    } else {
        Settled::Done
    })
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
              WHERE run_id = ?1 AND state = 'queued' AND mode = 'fresh'
                AND (check_state IS NULL OR check_state = 'passed')",
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
    // D-062 r3 item 5: never a child on a claimed pane's behalf.
    if crate::herdr_claims::claimed_row(&tx, &chapter_id)? {
        return Ok(Err("claimed-pane"));
    }
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
                       AND (check_state IS NULL OR check_state = 'passed')
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
                        "failed",
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

    static CHECK_NODE_BUSY: Mutex<Option<HashMap<String, u32>>> = Mutex::new(None);
    static CHECK_SETTLE_FAILURES: Mutex<Option<HashMap<String, u32>>> = Mutex::new(None);
    static CHECK_REPORT_FAILURES: Mutex<Option<HashMap<String, u32>>> = Mutex::new(None);

    fn arm(slot: &Mutex<Option<HashMap<String, u32>>>, id: &str, times: u32) {
        let mut slot = slot.lock().unwrap_or_else(|poison| poison.into_inner());
        let armed = slot.get_or_insert_with(HashMap::new);
        if times == 0 {
            armed.remove(id);
        } else {
            armed.insert(id.to_owned(), times);
        }
    }

    fn take(slot: &Mutex<Option<HashMap<String, u32>>>, id: &str) -> bool {
        let mut slot = slot.lock().unwrap_or_else(|poison| poison.into_inner());
        let Some(armed) = slot.as_mut() else {
            return false;
        };
        match armed.get_mut(id) {
            Some(times) => {
                *times -= 1;
                if *times == 0 {
                    armed.remove(id);
                }
                true
            }
            None => false,
        }
    }

    /// Make the next `times` applies of the check report of `id` (a run id,
    /// or a wake id for every run of it) fail; 0 disarms.
    pub fn arm_check_settle_failure(run_id: &str, times: u32) {
        arm(&CHECK_SETTLE_FAILURES, run_id, times);
    }

    pub(crate) fn take_check_settle_failure(run_id: &str) -> bool {
        take(&CHECK_SETTLE_FAILURES, run_id)
    }

    /// Make the next `times` writes queueing the check report of `id` (a run
    /// id, or a wake id) fail; 0 disarms.
    pub fn arm_check_report_failure(run_id: &str, times: u32) {
        arm(&CHECK_REPORT_FAILURES, run_id, times);
    }

    pub(crate) fn take_check_report_failure(run_id: &str) -> bool {
        take(&CHECK_REPORT_FAILURES, run_id)
    }

    /// Make the next `times` Node calls of the check of `id` (a run id, or a
    /// wake id for every run of it) answer NODE_BUSY (refused before the
    /// frame was queued); 0 disarms.
    pub fn arm_check_node_busy(id: &str, times: u32) {
        arm(&CHECK_NODE_BUSY, id, times);
    }

    /// Whether this Node call of the check of `id` is an armed NODE_BUSY.
    pub(crate) fn take_check_node_busy(id: &str) -> bool {
        take(&CHECK_NODE_BUSY, id)
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

    /// r2: the Hub waits longer than a Node can take for a check (its
    /// timeout plus the Node's bounded overhead), and a check its Node never
    /// received is asked again after a bounded backoff.
    #[test]
    fn the_check_wait_outlasts_the_node_and_retries_back_off_to_a_bound() {
        for timeout in [1, 30, WAKE_CHECK_TIMEOUT_MAX_SECS as u64] {
            assert!(
                check_rpc_wait(timeout).as_secs()
                    > timeout + remuda_protocol::WAKE_CHECK_NODE_OVERHEAD_SECS,
                "{timeout}"
            );
        }
        assert_eq!(check_retry_secs(1), 1);
        assert_eq!(check_retry_secs(2), 2);
        assert_eq!(check_retry_secs(7), 60);
        assert_eq!(check_retry_secs(i64::MAX), 60);
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
        assert!(matches!(
            check_add(
                &request(json!({"kind": "when", "factKind": "wake-failed"})),
                now
            ),
            Err(WakeError::BadRequest(_))
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

    fn with_check(check: Value) -> WakeAddRequest {
        let mut add = request(json!({"kind": "every", "everySecs": 180}));
        add.check = Some(serde_json::from_value(check).unwrap());
        add
    }

    /// §11.2: the timeout is mandatory and bounded, the command bounded, and
    /// the check is part of what a repeat must match.
    #[test]
    fn a_check_is_bounded_and_its_timeout_is_mandatory() {
        let now = at("2026-11-04T10:00:00.000Z");
        let checked = check_add(
            &with_check(json!({"command": " test -s q ", "hostId": "hst_a", "timeoutSecs": 30})),
            now,
        )
        .unwrap();
        assert_eq!(
            checked.check,
            Some(CheckedCheck {
                command: "test -s q".into(),
                host_id: Some("hst_a".into()),
                timeout_secs: 30
            })
        );
        let other = check_add(
            &with_check(json!({"command": "test -s r", "hostId": "hst_a", "timeoutSecs": 30})),
            now,
        )
        .unwrap();
        assert_ne!(
            checked.request_json, other.request_json,
            "a changed command is a new add"
        );
        for bad in [
            json!({"command": "true"}),
            json!({"command": "true", "timeoutSecs": 0}),
            json!({"command": "true", "timeoutSecs": WAKE_CHECK_TIMEOUT_MAX_SECS + 1}),
            json!({"command": "true", "timeoutSecs": -5}),
            json!({"timeoutSecs": 5}),
            json!({"command": "   ", "timeoutSecs": 5}),
            json!({"command": "x".repeat(WAKE_CHECK_COMMAND_MAX_CHARS + 1), "timeoutSecs": 5}),
            json!({"command": "true", "timeoutSecs": 5, "hostId": "two words"}),
        ] {
            assert!(
                matches!(
                    check_add(&with_check(bad.clone()), now),
                    Err(WakeError::BadRequest(_))
                ),
                "{bad}"
            );
        }
        let longest = "x".repeat(WAKE_CHECK_COMMAND_MAX_CHARS);
        assert!(
            check_add(
                &with_check(json!({"command": longest, "timeoutSecs": 3600})),
                now
            )
            .is_ok()
        );
    }

    /// A credential in a check's command is refused as one, before any
    /// other check (a missing timeout included), and never echoed.
    #[test]
    fn a_credential_in_a_check_is_refused_as_one() {
        let now = at("2026-11-04T10:00:00.000Z");
        let secret = ["sk", "-ant-api03-", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"].concat();
        let refused = check_add(
            &with_check(json!({"command": format!("curl -H 'x-api-key: {secret}' q")})),
            now,
        );
        let Err(WakeError::SecretRejected { field, rule }) = refused else {
            panic!("expected a secret refusal");
        };
        assert_eq!(field, "check.command");
        let body = WakeError::SecretRejected { field, rule }.into_response();
        assert_eq!(body.status(), StatusCode::UNPROCESSABLE_ENTITY);
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
