//! D-057 §6.3 / §6.3.1 (ma-restart): the C1 supervisor.
//!
//! The supervisor runs on the Hub's 1 s scheduler tick and once at boot. It
//! evaluates lineages that are `starting` or `running` by state (the derived
//! `host-offline` is one of those) and acts only on Node-attested
//! process-end evidence (OA6) bound to the lineage's CURRENT chapter at the
//! live generation:
//!
//! * the process ended by itself after reporting running → `process-exit`;
//! * the chapter never started (a rejected or never-acknowledged create, a
//!   journaled start failure, or an end before the first running event) →
//!   `start-failed`;
//! * the Node's epoch changed and its inventory no longer holds the chapter
//!   → `node-restarted`.
//!
//! Host loss is contact loss, never evidence: a host-lost row carries no
//! `ended_at`, so it is not a candidate, and a same-epoch reconnect revives
//! it. A close fences at admission and pauses the lineage, so an exit that
//! follows a close finds it paused and is ignored (a close always wins).
//!
//! **Exactly one decision per failed chapter.** Every decision is F (§7.2)
//! with a generation CAS — with a successor (a restart), or without one
//! (`paused{restart-cap}` at the cap, `paused{process-exit}` when the
//! lineage has no `restart.onProcessLoss`) — and it records its row in
//! `lineage_restart_decisions`, keyed by the failed chapter id, in the same
//! transaction. Inside that transaction the decision re-checks that the
//! failed chapter is still current at the expected generation, still carries
//! its evidence and has no row yet, so a second evaluation (another tick, a
//! Hub restart, a racing Resume or close) writes nothing.
//!
//! **The cap** counts restart decisions of every cause in the last hour.
//! **Recovery source** (§6.3.1): the failed chapter's native session, else
//! the newest earlier chapter's, else a fresh launch from the original create
//! spec; the successor runs claude-sdk on the same host. **Launch
//! authority** (OA5): the successor's create carries the launch origin
//! recorded for the lineage's first chapter; its actor is the supervisor,
//! with the seating device beside it.

use crate::AppState;
use crate::fence::{FenceMode, FenceSpec, FenceTx, fence_core};
use crate::store::{
    InstanceRecord, LineageRecord, RestartPolicy, Settlement, Store, StoreError, immediate_tx,
    lifecycle_has_process_end_evidence, load_lineage,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};
use std::sync::Arc;

/// §6.3 causes, as recorded on the decision and as the successor's
/// `chapterCause`.
pub(crate) mod cause {
    /// The process ended by itself after reporting running.
    pub const PROCESS_EXIT: &str = "process-exit";
    /// The chapter never started.
    pub const START_FAILED: &str = "start-failed";
    /// The Node restarted and no longer holds the chapter.
    pub const NODE_RESTARTED: &str = "node-restarted";
}

/// `lineage_restart_decisions.outcome` values.
pub(crate) mod outcome {
    /// F with successor.
    pub const RESTARTED: &str = "restarted";
    /// F without successor at the cap: `paused{restart-cap}`.
    pub const RESTART_CAP: &str = "restart-cap";
    /// F without successor on a lineage without `restart.onProcessLoss`:
    /// `paused{process-exit}`.
    pub const PROCESS_EXIT: &str = "process-exit";
    /// No F: the Hub saw the end longer ago than [`super::DECISION_WINDOW_MINUTES`]
    /// (typically history found by the first boot after an upgrade). The
    /// chapter is not relaunched; its owner can Resume it.
    pub const EXPIRED: &str = "expired";
}

/// An end the Hub saw longer ago than this is recorded `expired` instead of
/// relaunched. The supervisor decides a fresh end within a tick, so only a
/// Hub that was down (or ran without the supervisor) this long finds one;
/// the first boot after the upgrade must not relaunch long-dead seats in a
/// burst, while a Hub restart after a short outage still decides.
pub(crate) const DECISION_WINDOW_MINUTES: i64 = 30;

/// The supervisor's actor in fence and audit records.
const SUPERVISOR: &str = "hub-supervisor";

/// Idempotent DDL; no `user_version` rung. Also lifts lineages whose current
/// chapter already runs to the stored `running` state (rows written before
/// the projection below existed).
pub(crate) fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS lineage_restart_decisions (
            failed_instance_id TEXT PRIMARY KEY,
            lineage_id TEXT NOT NULL,
            failed_generation INTEGER NOT NULL,
            cause TEXT NOT NULL,
            outcome TEXT NOT NULL,
            successor_instance_id TEXT,
            successor_generation INTEGER,
            recovery TEXT,
            recovery_session_id TEXT,
            create_command_id TEXT,
            notice_command_id TEXT,
            fence_id TEXT,
            reason TEXT,
            push_state TEXT NOT NULL DEFAULT 'none',
            decided_at TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS lineage_restart_decisions_lineage
            ON lineage_restart_decisions(lineage_id, decided_at);
         CREATE TABLE IF NOT EXISTS lineage_c1_pushes (
            fence_id TEXT PRIMARY KEY,
            lineage_id TEXT NOT NULL,
            instance_id TEXT NOT NULL,
            kind TEXT NOT NULL,
            title TEXT NOT NULL,
            body TEXT NOT NULL,
            created_at TEXT NOT NULL,
            sent_at TEXT,
            recipients INTEGER
         );
         CREATE INDEX IF NOT EXISTS lineage_c1_pushes_unsent
            ON lineage_c1_pushes(sent_at, created_at);",
    )?;
    // When THIS Hub first held process-end evidence for a chapter, on its
    // own clock, written once (r3). A trigger rather than a call per write
    // path, so every path that records an end (journal projection, failed
    // create, epoch reconcile, stale-create sweep, daemon reconcile, delete
    // stop) is covered. The predicate mirrors
    // `store::lifecycle_has_process_end_evidence`: a host-contact-lost
    // `exited` row without `ended_at` is contact loss, not an end. A row
    // that returns to a live lifecycle loses the stamp.
    crate::store::ensure_column(conn, "instances", "end_seen_at", "TEXT")?;
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS instances_end_seen;
         CREATE TRIGGER instances_end_seen AFTER UPDATE ON instances
         WHEN NEW.end_seen_at IS NULL AND (
              NEW.lifecycle = 'closed'
           OR (NEW.lifecycle = 'exited'
               AND (NEW.ended_at IS NOT NULL
                    OR COALESCE(NEW.last_error, '') <> 'host-contact-lost'))
           OR (NEW.lifecycle = 'failed'
               AND (NEW.ended_at IS NOT NULL
                    OR LOWER(COALESCE(NEW.last_error, '')) = 'create-never-acknowledged'
                    OR LOWER(COALESCE(NEW.last_error, '')) LIKE '%start-fail%'
                    OR LOWER(COALESCE(NEW.last_error, '')) LIKE '%start failed%'
                    OR LOWER(COALESCE(NEW.last_error, '')) LIKE '%never started%')))
         BEGIN
           UPDATE instances SET end_seen_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = NEW.id AND end_seen_at IS NULL;
         END;
         DROP TRIGGER IF EXISTS instances_end_unseen;
         CREATE TRIGGER instances_end_unseen AFTER UPDATE OF lifecycle ON instances
         WHEN NEW.end_seen_at IS NOT NULL
          AND NEW.lifecycle NOT IN ('exited', 'failed', 'closed')
         BEGIN
           UPDATE instances SET end_seen_at = NULL WHERE id = NEW.id;
         END;
         CREATE TABLE IF NOT EXISTS hub_run_marker (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            last_alive_at TEXT NOT NULL
         );",
    )?;
    conn.execute(
        "UPDATE lineages SET state = 'running'
          WHERE state = 'starting'
            AND EXISTS (SELECT 1 FROM instances i
                         WHERE i.id = lineages.current_instance_id
                           AND i.generation = lineages.generation
                           AND i.fenced_at IS NULL
                           AND i.lifecycle IN ('ready', 'running'))",
        [],
    )?;
    Ok(())
}

/// §6.2 stored state: a lineage moves `starting` → `running` when its current
/// chapter at the live generation reports running. Runs in the journal
/// append transaction; the cause table reads it (an end before the first
/// running event is a start failure).
pub(crate) fn note_chapter_running(conn: &Connection, instance_id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE lineages SET state = 'running', updated_at = ?2
          WHERE current_instance_id = ?1 AND state = 'starting'
            AND EXISTS (SELECT 1 FROM instances i
                         WHERE i.id = ?1 AND i.generation = lineages.generation
                           AND i.fenced_at IS NULL
                           AND i.lifecycle IN ('ready', 'running'))",
        params![instance_id, crate::config::now_rfc3339()],
    )?;
    Ok(())
}

/// The §6.3 cause of a failed chapter's evidence.
fn classify(lineage_state: &str, lifecycle: &str, last_error: Option<&str>) -> &'static str {
    if last_error == Some(crate::ws::NODE_EPOCH_CHANGED) {
        return cause::NODE_RESTARTED;
    }
    if lineage_state == "starting"
        || (lifecycle == "failed"
            && last_error.is_some_and(crate::store::is_attested_launch_failure_marker))
    {
        return cause::START_FAILED;
    }
    cause::PROCESS_EXIT
}

/// A C1 decision with successor, checked and recorded inside the
/// continuation transaction (see [`crate::store::ContinuationResumeRequest`]).
#[derive(Clone, Debug)]
pub struct RestartDecision {
    /// The failed chapter: the decision key.
    pub failed_instance_id: String,
    /// §6.3 cause.
    pub cause: &'static str,
    /// §6.3.1 recovery source (`chapter-session` / `earlier-session` /
    /// `fresh`).
    pub recovery: &'static str,
    /// The native session the successor resumes, if any.
    pub recovery_session_id: Option<String>,
}

/// `(lifecycle, last_error, ended_at, generation, fenced_at)` of a chapter.
type ChapterEvidenceRow = (String, Option<String>, Option<String>, i64, Option<String>);

/// Whether a decision about `failed_instance_id` may still be made, read
/// inside the deciding transaction: the chapter is the lineage's current
/// chapter at `expected_generation`, the lineage is not paused, the chapter
/// is not fenced and carries process-end evidence, and no decision is keyed
/// by it yet.
pub(crate) fn decision_open(
    conn: &Connection,
    lineage_id: &str,
    expected_generation: i64,
    failed_instance_id: &str,
) -> Result<bool, StoreError> {
    let decided = conn
        .query_row(
            "SELECT 1 FROM lineage_restart_decisions WHERE failed_instance_id = ?1",
            params![failed_instance_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if decided {
        return Ok(false);
    }
    let Some(lineage) = load_lineage(conn, lineage_id)? else {
        return Ok(false);
    };
    if lineage.current_instance_id != failed_instance_id
        || lineage.generation != expected_generation
        || lineage.state == "paused"
    {
        return Ok(false);
    }
    let row: Option<ChapterEvidenceRow> = conn
        .query_row(
            "SELECT lifecycle, last_error, ended_at, generation, fenced_at
               FROM instances WHERE id = ?1",
            params![failed_instance_id],
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
    let Some((lifecycle, last_error, ended_at, generation, fenced_at)) = row else {
        return Ok(false);
    };
    Ok(generation == lineage.generation
        && fenced_at.is_none()
        && lifecycle_has_process_end_evidence(
            &lifecycle,
            last_error.as_deref(),
            ended_at.as_deref(),
        ))
}

/// Insert one decision row (the key is the failed chapter id).
#[allow(clippy::too_many_arguments)]
fn insert_decision(
    conn: &Connection,
    failed_instance_id: &str,
    lineage_id: &str,
    failed_generation: i64,
    cause: &str,
    outcome: &str,
    fence_id: Option<&str>,
    reason: Option<&str>,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO lineage_restart_decisions
            (failed_instance_id, lineage_id, failed_generation, cause, outcome,
             fence_id, reason, decided_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            failed_instance_id,
            lineage_id,
            failed_generation,
            cause,
            outcome,
            fence_id,
            reason,
            crate::config::now_rfc3339(),
        ],
    )?;
    Ok(())
}

/// Record a restart decision (F with successor), inside the continuation
/// transaction, after the successor and its create are inserted. The restart
/// notice's command id is chosen and its row persisted here too (§6.4), so a
/// Hub crash can never send it twice: it is queued for the successor with
/// origin `hub`, carrying the successor's lineage and generation and no
/// device id (§7.1), and forwarded once the successor's host acknowledged
/// the fence and accepted the create ([`drive_notices`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_restart(
    conn: &Connection,
    decision: &RestartDecision,
    lineage_id: &str,
    failed_generation: i64,
    successor_id: &str,
    successor_generation: i64,
    host_id: &str,
    create_command_id: &str,
    fence_id: &str,
) -> Result<(), StoreError> {
    insert_decision(
        conn,
        &decision.failed_instance_id,
        lineage_id,
        failed_generation,
        decision.cause,
        outcome::RESTARTED,
        Some(fence_id),
        None,
    )?;
    conn.execute(
        "UPDATE lineage_restart_decisions
            SET successor_instance_id = ?2, successor_generation = ?3, recovery = ?4,
                recovery_session_id = ?5, create_command_id = ?6
          WHERE failed_instance_id = ?1",
        params![
            decision.failed_instance_id,
            successor_id,
            successor_generation,
            decision.recovery,
            decision.recovery_session_id,
            create_command_id,
        ],
    )?;
    let notice_id = crate::config::new_id("cmd").map_err(|e| StoreError::Id(e.to_string()))?;
    let text = render_notice(conn, &decision.failed_instance_id)?;
    let now = crate::config::now_rfc3339();
    conn.execute(
        "INSERT INTO commands
            (id, instance_id, host_id, operation, state, resolution, forwarded,
             payload_json, idempotency_key, created_at, updated_at,
             initiator_instance_id, initiator_lineage_id, initiator_generation)
         VALUES (?1, ?2, ?3, 'instance.send', 'queued', 'clear', 0, ?4, NULL, ?5, ?5,
                 ?2, ?6, ?7)",
        params![
            notice_id,
            successor_id,
            host_id,
            notice_payload(successor_id, &text).to_string(),
            now,
            lineage_id,
            successor_generation,
        ],
    )?;
    conn.execute(
        "UPDATE lineage_restart_decisions SET notice_command_id = ?2 WHERE failed_instance_id = ?1",
        params![decision.failed_instance_id, notice_id],
    )?;
    queue_push(
        conn,
        fence_id,
        lineage_id,
        successor_id,
        push_kind::RESTARTED,
        Some(decision.cause),
    )?;
    Ok(())
}

/// §8.5 C1 push kinds.
pub(crate) mod push_kind {
    /// A restart decision: "Main restarted (cause)".
    pub const RESTARTED: &str = "restarted";
    /// The cap: "Main paused: restart cap".
    pub const RESTART_CAP: &str = "restart-cap";
    /// A close by the chapter itself: "Main closed itself".
    pub const SELF: &str = "self";
    /// A close by an Agent ancestor.
    pub const ANCESTOR: &str = "ancestor";
    /// The successor could not be prepared (its profile or route is gone):
    /// "Main paused: could not restart".
    pub const RESTART_FAILED: &str = "restart-failed";
}

/// Record the C1 push of one F, in F's transaction (one per fence, so a
/// retried decision or close can never add a second). It is sent after
/// commit, and by the supervisor's next evaluation if the Hub stopped first.
/// A pause from a Human device and `paused{process-exit}` (today's exited
/// push) record none.
pub(crate) fn queue_push(
    conn: &Connection,
    fence_id: &str,
    lineage_id: &str,
    instance_id: &str,
    kind: &str,
    cause_name: Option<&str>,
) -> Result<(), StoreError> {
    // The seat (the chapter that addresses the owner) is "Main"; any other
    // continuity lineage is named by its title.
    let (title, grants): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT title, grants_json FROM instances WHERE id = ?1",
            params![instance_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .unwrap_or_default();
    let is_seat = grants
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
        .is_some_and(|grants| grants.iter().any(|grant| grant == "address-owner"));
    let label = if is_seat {
        "Main".to_owned()
    } else {
        title
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| "Session".to_owned())
    };
    let (headline, body) = match kind {
        push_kind::RESTARTED => {
            let why = match cause_name {
                Some(cause::START_FAILED) => "start failed",
                Some(cause::NODE_RESTARTED) => "Node restarted",
                _ => "process exited",
            };
            (
                format!("{label} restarted ({why})"),
                "A new chapter is starting; the Hub's restart notice is its first input."
                    .to_owned(),
            )
        }
        push_kind::RESTART_CAP => (
            format!("{label} paused: restart cap"),
            "It reached its restarts per hour and stays paused until you resume it.".to_owned(),
        ),
        push_kind::SELF => (
            format!("{label} closed itself"),
            "It is paused until you resume it.".to_owned(),
        ),
        push_kind::RESTART_FAILED => (
            format!("{label} paused: could not restart"),
            "Its relaunch could not be prepared; it stays paused until you resume it.".to_owned(),
        ),
        _ => (
            format!("{label} paused by its parent agent"),
            "It is paused until it is resumed.".to_owned(),
        ),
    };
    conn.execute(
        "INSERT OR IGNORE INTO lineage_c1_pushes
            (fence_id, lineage_id, instance_id, kind, title, body, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            fence_id,
            lineage_id,
            instance_id,
            kind,
            headline,
            body,
            crate::config::now_rfc3339()
        ],
    )?;
    Ok(())
}

/// One unsent C1 push.
#[derive(Debug)]
struct C1Push {
    fence_id: String,
    lineage_id: String,
    instance_id: String,
    title: String,
    body: String,
}

/// Send every recorded, unsent C1 push (`recipients IS NULL`) through the
/// existing push fanout. A row is marked sent — `sent_at` and its recipient
/// count — only AFTER the fanout delivered it, or found nothing to deliver
/// to (push off, a device following the chapter, no live subscription). A
/// failed fanout leaves it unsent for the next evaluation, including the boot
/// evaluation after a Hub restart; a crash between send and mark resends it,
/// and the lineage tag makes the resend replace the first notification.
/// Drives are serialized in-process, so two never send the same row.
pub(crate) async fn drive_pushes(state: &AppState) {
    let _drive = state.restart.pushes.lock().await;
    let pushes = match state
        .store
        .read("c1_pushes_unsent", |conn| {
            let mut stmt = conn.prepare(
                "SELECT fence_id, lineage_id, instance_id, title, body
                   FROM lineage_c1_pushes WHERE recipients IS NULL ORDER BY created_at",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(C1Push {
                        fence_id: row.get(0)?,
                        lineage_id: row.get(1)?,
                        instance_id: row.get(2)?,
                        title: row.get(3)?,
                        body: row.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "could not read unsent C1 pushes");
            return;
        }
    };
    for push in pushes {
        let fanout = crate::alerts::c1_push(
            state,
            &push.lineage_id,
            &push.instance_id,
            &push.title,
            &push.body,
        )
        .await;
        let Some(recipients) = fanout.recipients() else {
            tracing::warn!(lineage_id = %push.lineage_id, title = %push.title,
                "C1 push failed; it stays unsent and is retried");
            continue;
        };
        tracing::info!(lineage_id = %push.lineage_id, title = %push.title, recipients, "C1 push");
        if let Err(error) = state
            .store
            .run_named("c1_push_sent", move |conn| {
                conn.execute(
                    "UPDATE lineage_c1_pushes SET sent_at = ?2, recipients = ?3
                      WHERE fence_id = ?1 AND recipients IS NULL",
                    params![
                        push.fence_id,
                        crate::config::now_rfc3339(),
                        i64::try_from(recipients).unwrap_or(i64::MAX)
                    ],
                )?;
                Ok(())
            })
            .await
        {
            tracing::warn!(%error, "could not mark a C1 push sent; it is resent");
        }
    }
}

/// Whether `instance_id`'s generic exited push is replaced by a C1 push: its
/// lineage restarts on process loss (§8.5).
pub(crate) async fn c1_replaces_exit_push(state: &AppState, instance_id: &str) -> bool {
    let Ok(Some(instance)) = state.store.get_instance_read(instance_id.to_owned()).await else {
        return false;
    };
    instance
        .restart
        .and_then(|value| serde_json::from_value::<RestartPolicy>(value).ok())
        .is_some_and(|policy| policy.on_process_loss)
}

/// Settlement reason of a restart notice whose successor create was
/// rejected or withheld for a non-fence reason.
pub(crate) const NOTICE_ORPHANED_REASON: &str = "successor-create-rejected";

/// The fixed tag the restart notice opens with (§6.4).
pub(crate) const NOTICE_TAG: &str = "<remuda-restart>";

/// The notice's `instance.send` payload: Hub-written input, origin `hub`.
fn notice_payload(successor_id: &str, text: &str) -> Value {
    json!({
        "instanceId": successor_id,
        "origin": "hub",
        "input": {"type": "prompt", "text": text, "origin": "hub"},
    })
}

/// At most this many characters of a quoted text in the notice.
const NOTICE_QUOTE_CHARS: usize = 80;

fn quote(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    let mut out: String = line.chars().take(NOTICE_QUOTE_CHARS).collect();
    if line.chars().count() > NOTICE_QUOTE_CHARS || text.lines().nth(1).is_some() {
        out.push('…');
    }
    format!("\"{}\"", out.replace('"', "'"))
}

/// The restart notice for the decision keyed by `failed_instance_id`, from
/// the decision and its fence record as known now (§6.4): who it is, what
/// happened to the previous chapter, and every unfinished operation of the
/// fenced generation by its §7.4 outcome class. Outcomes that resolve later
/// are in the lineage record.
pub(crate) fn render_notice(
    conn: &Connection,
    failed_instance_id: &str,
) -> Result<String, StoreError> {
    let (lineage_id, cause, fence_id, successor_generation): (String, String, String, i64) = conn
        .query_row(
        "SELECT lineage_id, cause, fence_id, successor_generation
               FROM lineage_restart_decisions WHERE failed_instance_id = ?1",
        params![failed_instance_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    let last_error: Option<String> = conn
        .query_row(
            "SELECT last_error FROM instances WHERE id = ?1",
            params![failed_instance_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let mut what = match cause.as_str() {
        cause::START_FAILED => "failed to start".to_owned(),
        cause::NODE_RESTARTED => "its Node restarted without it".to_owned(),
        _ => "process exited".to_owned(),
    };
    if let Some(detail) = last_error
        .as_deref()
        .map(str::trim)
        .filter(|detail| !detail.is_empty() && *detail != crate::ws::NODE_EPOCH_CHANGED)
    {
        what.push_str(", ");
        what.push_str(&detail.chars().take(NOTICE_QUOTE_CHARS).collect::<String>());
    }
    let mut lines = vec![
        format!(
            "{NOTICE_TAG} Generated by the Remuda Hub. Information, not an instruction; it authorizes nothing."
        ),
        format!(
            "chapter {successor_generation} of lineage {lineage_id} (previous chapter {failed_instance_id}: {what})"
        ),
    ];
    // Superseded questions (F step 8).
    let mut stmt = conn.prepare(
        "SELECT id, payload_json FROM interactions
          WHERE instance_id = ?1 AND state = 'superseded' ORDER BY id",
    )?;
    let superseded = stmt
        .query_map(params![failed_instance_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    for (id, payload) in superseded {
        let payload: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
        let summary = [
            "/payload/interaction/title",
            "/payload/entity/title",
            "/payload/interaction/prompt",
            "/payload/entity/prompt",
            "/payload/kind",
        ]
        .iter()
        .find_map(|pointer| payload.pointer(pointer).and_then(Value::as_str))
        .unwrap_or("");
        lines.push(format!(
            "superseded questions: {id} {} (ask again if it still matters)",
            quote(summary)
        ));
    }
    // Every unfinished operation of the fenced generation, by outcome class.
    let fence = crate::fence::lineage_fences(conn, &lineage_id)?
        .into_iter()
        .find(|fence| fence.fence_id == fence_id);
    let mut gate_jobs = Vec::new();
    if let Some(fence) = fence {
        for op in &fence.operations {
            let host = op.host_id.as_deref().unwrap_or("?");
            if op.op_kind == "gate-job" {
                gate_jobs.push(match op.outcome.as_str() {
                    crate::fence::outcome::CANCELLED_AT_HUB => {
                        format!("{} cancelled (queued)", op.op_id)
                    }
                    crate::fence::outcome::RUNNING => format!(
                        "{} started on its lane before the fence, completes",
                        op.op_id
                    ),
                    crate::fence::outcome::RAN => format!("{} ran on its lane", op.op_id),
                    crate::fence::outcome::CANCELLED_BY_NODE
                    | crate::fence::outcome::REFUSED_BY_NODE => {
                        format!("{} cancelled by host {host} at the fence", op.op_id)
                    }
                    crate::fence::outcome::WITHHELD => {
                        format!("{} withheld at the Hub, never sent", op.op_id)
                    }
                    _ => format!("{} claimed, start unknown", op.op_id),
                });
                continue;
            }
            let (target, text) = op_target(conn, &op.op_kind, &op.op_id, host)?;
            let operation = op.operation.as_deref().unwrap_or("?");
            let subject = format!("{} {operation} -> {target}", op.op_id);
            let line = match op.outcome.as_str() {
                crate::fence::outcome::CANCELLED_AT_HUB => match text {
                    Some(text) => {
                        format!("cancelled, never left the Hub: {subject} {}", quote(&text))
                    }
                    None => format!("cancelled, never left the Hub: {subject}"),
                },
                crate::fence::outcome::WITHHELD => {
                    format!("withheld at the Hub, never sent: {subject}")
                }
                crate::fence::outcome::CANCELLED_BY_NODE => {
                    format!("cancelled by host {host} when it applied the fence: {subject}")
                }
                crate::fence::outcome::REFUSED_BY_NODE => {
                    format!("refused by host {host} after it applied the fence: {subject}")
                }
                crate::fence::outcome::RAN => {
                    format!("ran before its host applied the fence: {subject}")
                }
                _ if fence.pending_hosts.iter().any(|pending| pending == host) => {
                    let since: Option<String> = conn
                        .query_row(
                            "SELECT offline_since FROM hosts WHERE id = ?1 AND state != 'online'",
                            params![host],
                            |row| row.get(0),
                        )
                        .optional()?
                        .flatten();
                    let when = since.map_or_else(
                        || "not acknowledged yet".to_owned(),
                        |since| format!("offline since {since}"),
                    );
                    format!(
                        "may still run, host {host} has not applied the fence ({when}): {subject}"
                    )
                }
                _ => format!(
                    "unknown, host {host} applied the fence but cannot tell after a crash: {subject}"
                ),
            };
            lines.push(line);
        }
    }
    if !gate_jobs.is_empty() {
        lines.push(format!("gate jobs: {}", gate_jobs.join("; ")));
    }
    lines.push(format!("later outcomes: GET /v1/lineages/{lineage_id}"));
    lines.push(format!(
        "previous chapter: remuda instance read {failed_instance_id}"
    ));
    lines.push("</remuda-restart>".to_owned());
    Ok(lines.join("\n"))
}

/// What an operation in the notice was addressed to, and a send's text.
fn op_target(
    conn: &Connection,
    kind: &str,
    op_id: &str,
    host: &str,
) -> Result<(String, Option<String>), StoreError> {
    if kind != "command" {
        return Ok((format!("host {host}"), None));
    }
    let row: Option<(Option<String>, String)> = conn
        .query_row(
            "SELECT instance_id, payload_json FROM commands WHERE id = ?1",
            params![op_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((instance, payload)) = row else {
        return Ok((format!("host {host}"), None));
    };
    let payload: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
    let text = payload
        .pointer("/input/text")
        .or_else(|| payload.get("prompt"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok((
        instance.map_or_else(|| format!("host {host}"), |id| format!("instance {id}")),
        text,
    ))
}

/// A restart notice whose successor create the Node accepted and that has
/// not been forwarded yet.
#[derive(Debug)]
struct NoticeDue {
    failed_instance_id: String,
    notice_command_id: String,
    host_id: String,
}

impl Store {
    /// Restart notices still queued whose successor's create has been
    /// accepted by its Node (so the notice follows it on the link).
    async fn notices_due(&self) -> Result<Vec<NoticeDue>, StoreError> {
        self.read("restart_notices_due", |conn| {
            let mut stmt = conn.prepare(
                "SELECT d.failed_instance_id, n.id, n.host_id
                   FROM lineage_restart_decisions d
                   JOIN commands n ON n.id = d.notice_command_id
                   JOIN commands c ON c.id = d.create_command_id
                  WHERE n.state = 'queued' AND n.forwarded = 0
                    AND c.forwarded = 1 AND c.state != 'queued'
                    AND COALESCE(c.settlement_outcome, '') NOT IN ('rejected', 'withheld')
                  ORDER BY d.decided_at",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(NoticeDue {
                        failed_instance_id: row.get(0)?,
                        notice_command_id: row.get(1)?,
                        host_id: row.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    /// Settle every still-queued restart notice whose successor create was
    /// rejected or withheld for a reason other than a fence: the notice can
    /// never follow that create. Settled `cancelled`, reason
    /// `successor-create-rejected`. (A fenced create's notice was already
    /// cancelled by the F that fenced it.) Returns the settled command ids.
    async fn settle_orphan_notices(&self) -> Result<Vec<String>, StoreError> {
        self.run_named("restart_orphan_notices", |conn| {
            let tx = immediate_tx(conn)?;
            let ids: Vec<String> = {
                let mut stmt = tx.prepare(
                    "SELECT n.id FROM lineage_restart_decisions d
                       JOIN commands n ON n.id = d.notice_command_id
                       JOIN commands c ON c.id = d.create_command_id
                      WHERE n.state = 'queued' AND n.forwarded = 0
                        AND c.state = 'settled'
                        AND c.settlement_outcome IN ('rejected', 'withheld')
                        AND COALESCE(c.settlement_reason, '') != ?1",
                )?;
                stmt.query_map(params![remuda_protocol::FENCED_REASON], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            let now = crate::config::now_rfc3339();
            for id in &ids {
                tx.execute(
                    "UPDATE commands
                        SET state = 'settled', resolution = 'clear',
                            settlement_outcome = 'cancelled', settlement_reason = ?2,
                            updated_at = ?3
                      WHERE id = ?1 AND state = 'queued' AND forwarded = 0",
                    params![id, NOTICE_ORPHANED_REASON, now],
                )?;
            }
            tx.commit()?;
            Ok(ids)
        })
        .await
    }

    /// Restart notices still queued for a host without fence support: §6.4
    /// sends hub-origin input only to Nodes that advertise it.
    async fn notices_for_incapable_hosts(&self) -> Result<Vec<(String, String)>, StoreError> {
        self.read("restart_notices_incapable", |conn| {
            let mut stmt = conn.prepare(
                "SELECT n.id, n.host_id FROM lineage_restart_decisions d
                   JOIN commands n ON n.id = d.notice_command_id
                   JOIN hosts h ON h.id = n.host_id
                  WHERE n.state = 'queued' AND n.forwarded = 0
                    AND h.state = 'online'
                    AND COALESCE(json_extract(h.capabilities_json, '$.lineageFence'), 0) != 1",
            )?;
            let rows = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    /// Re-render a still-unsent notice with the outcomes known now (§6.4:
    /// "as known when it is sent"). A row that left the queue keeps the text
    /// it was sent with: the replay identity is the command id.
    async fn refresh_notice(
        &self,
        failed_instance_id: String,
        notice_command_id: String,
    ) -> Result<Option<crate::store::CommandRecord>, StoreError> {
        self.run_named("restart_refresh_notice", move |conn| {
            let tx = immediate_tx(conn)?;
            let successor: Option<String> = tx
                .query_row(
                    "SELECT instance_id FROM commands
                      WHERE id = ?1 AND state = 'queued' AND forwarded = 0",
                    params![notice_command_id],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            let Some(successor) = successor else {
                return Ok(None);
            };
            let text = render_notice(&tx, &failed_instance_id)?;
            tx.execute(
                "UPDATE commands SET payload_json = ?2, updated_at = ?3 WHERE id = ?1",
                params![
                    notice_command_id,
                    notice_payload(&successor, &text).to_string(),
                    crate::config::now_rfc3339()
                ],
            )?;
            let command = crate::store::load_command(&tx, &notice_command_id)?;
            tx.commit()?;
            Ok(command)
        })
        .await
    }
}

/// Forward every restart notice that is due: its successor's host
/// acknowledged the fence (no unacknowledged fences: not gated), advertises
/// fence support, is live, and accepted the successor's create. A notice
/// for a host without fence support is settled `rejected(node-lacks-fence)`
/// and never sent. Each notice is sent once: its forward intent is the
/// replay identity.
pub(crate) async fn drive_notices(state: &AppState) {
    match state.store.settle_orphan_notices().await {
        Ok(ids) => {
            for id in ids {
                tracing::info!(notice = %id, "restart notice cancelled: its successor create was rejected");
            }
        }
        Err(error) => tracing::warn!(%error, "could not settle orphaned restart notices"),
    }
    if let Ok(incapable) = state.store.notices_for_incapable_hosts().await {
        for (command_id, host_id) in incapable {
            let error = crate::error::HubError::node_lacks_fence(&host_id);
            let (status, body) = error.status_code_and_body();
            if let Err(error) = state
                .store
                .reject_command_outcome(
                    command_id.clone(),
                    remuda_protocol::NODE_LACKS_FENCE_REASON.into(),
                    Some(i64::from(status)),
                    Some(body.to_string()),
                )
                .await
            {
                tracing::warn!(%error, %command_id, "could not settle a restart notice");
            }
        }
    }
    let due = match state.store.notices_due().await {
        Ok(due) => due,
        Err(error) => {
            tracing::warn!(%error, "could not read due restart notices");
            return;
        }
    };
    for notice in due {
        if state.nodes.kind_of(&notice.host_id).await.is_none() {
            continue;
        }
        let ready = state
            .store
            .read("restart_notice_host", {
                let host = notice.host_id.clone();
                move |conn| {
                    Ok(crate::fence::host_capable(conn, &host)?
                        && !crate::fence::host_gated(conn, &host)?)
                }
            })
            .await
            .unwrap_or(false);
        if !ready {
            continue;
        }
        let command = match state
            .store
            .refresh_notice(
                notice.failed_instance_id.clone(),
                notice.notice_command_id.clone(),
            )
            .await
        {
            Ok(Some(command)) => command,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(%error, "could not render a restart notice");
                continue;
            }
        };
        if let Err(error) = crate::http::forward_if_online(state, command, true).await {
            tracing::warn!(%error, notice = %notice.notice_command_id,
                "forwarding a restart notice failed");
        }
    }
}

/// One row of a lineage's restart-decision log (`GET /v1/lineages/{id}`).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DecisionView {
    pub failed_instance_id: String,
    pub failed_generation: i64,
    pub cause: String,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub successor_instance_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice_command_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fence_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub decided_at: String,
}

/// The lineage's restart-decision log, oldest first.
pub(crate) fn lineage_decisions(
    conn: &Connection,
    lineage_id: &str,
) -> Result<Vec<DecisionView>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT failed_instance_id, failed_generation, cause, outcome, successor_instance_id,
                recovery, notice_command_id, fence_id, reason, decided_at
           FROM lineage_restart_decisions WHERE lineage_id = ?1
          ORDER BY decided_at ASC, failed_generation ASC",
    )?;
    let rows = stmt
        .query_map(params![lineage_id], |row| {
            Ok(DecisionView {
                failed_instance_id: row.get(0)?,
                failed_generation: row.get(1)?,
                cause: row.get(2)?,
                outcome: row.get(3)?,
                successor_instance_id: row.get(4)?,
                recovery: row.get(5)?,
                notice_command_id: row.get(6)?,
                fence_id: row.get(7)?,
                reason: row.get(8)?,
                decided_at: row.get(9)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// A lineage whose current chapter carries process-end evidence and no
/// decision yet.
#[derive(Clone, Debug)]
struct Pending {
    lineage: LineageRecord,
    failed: InstanceRecord,
    cause: &'static str,
    /// When the Hub saw the end, if that is before the decision window.
    expired_seen: Option<String>,
}

/// What one evaluation decided (test seam and logs).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Decided {
    pub lineage_id: String,
    pub failed_instance_id: String,
    pub cause: String,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub successor_instance_id: Option<String>,
}

/// Serializes the tick's evaluations (a slow one is skipped by the next tick,
/// never stacked). Correctness never depends on it: the CAS and the decision
/// key make concurrent evaluations decide at most once.
#[derive(Clone, Default)]
pub(crate) struct SupervisorState {
    running: Arc<tokio::sync::Mutex<()>>,
    /// Serializes [`drive_pushes`], so one row is never sent twice at once.
    pushes: Arc<tokio::sync::Mutex<()>>,
    /// When this process last wrote the Hub run marker.
    alive_written: Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    /// Failed chapter → when its last transient preparation failure was
    /// logged (one warning per chapter per minute).
    prepare_logged: Arc<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>>,
    /// Test seam: the next N successor preparations fail with a transient
    /// store error.
    #[cfg(any(test, feature = "test-faults"))]
    prepare_faults: Arc<std::sync::atomic::AtomicUsize>,
}

impl SupervisorState {
    /// Whether a transient preparation failure of `failed` should be logged
    /// now: the first one, then at most once a minute.
    fn note_prepare_failure(&self, failed: &str) -> bool {
        let mut logged = self
            .prepare_logged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = std::time::Instant::now();
        match logged.get(failed) {
            Some(at) if now.duration_since(*at) < std::time::Duration::from_secs(60) => false,
            _ => {
                logged.insert(failed.to_owned(), now);
                true
            }
        }
    }
}

#[cfg(any(test, feature = "test-faults"))]
impl SupervisorState {
    /// Arm `count` transient preparation failures.
    pub(crate) fn fail_next_prepares(&self, count: usize) {
        self.prepare_faults
            .store(count, std::sync::atomic::Ordering::SeqCst);
    }

    fn take_prepare_fault(&self) -> Option<crate::error::HubError> {
        self.prepare_faults
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |left| left.checked_sub(1),
            )
            .ok()
            .map(|_| {
                crate::error::HubError::Store(StoreError::Id(
                    "injected transient store error".into(),
                ))
            })
    }
}

impl Store {
    /// Candidates: unpaused lineages whose current chapter, at the live
    /// generation and unfenced, is terminal-shaped and has no decision. The
    /// evidence predicate is applied by the caller.
    async fn restart_candidates(&self) -> Result<Vec<(String, String, String)>, StoreError> {
        self.read("restart_candidates", |conn| {
            // `seen` is when the Hub saw the end: the newest Hub-clock
            // journal row of the chapter, or its end stamp when that is
            // later (an end the Hub stamped itself), else its last update.
            let mut stmt = conn.prepare(
                "SELECT l.lineage_id, i.id,
                        COALESCE(NULLIF(MAX(COALESCE((SELECT MAX(j.observed_at) FROM journal j
                                                       WHERE j.instance_id = i.id), ''),
                                            COALESCE(i.ended_at, '')), ''),
                                 i.updated_at)
                   FROM lineages l
                   JOIN instances i ON i.id = l.current_instance_id
                  WHERE l.state IN ('starting', 'running')
                    AND i.generation = l.generation AND i.fenced_at IS NULL
                    AND i.lifecycle IN ('exited', 'failed', 'closed')
                    AND NOT EXISTS (SELECT 1 FROM lineage_restart_decisions d
                                     WHERE d.failed_instance_id = i.id)
                  ORDER BY l.lineage_id",
            )?;
            let rows = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    /// Record an `expired` decision without F, with the same in-transaction
    /// check as every decision. Returns whether it was recorded.
    async fn decide_expired(
        &self,
        lineage_id: String,
        expected_generation: i64,
        failed_instance_id: String,
        cause: &'static str,
        reason: String,
    ) -> Result<bool, StoreError> {
        self.run_named("restart_decide_expired", move |conn| {
            let tx = immediate_tx(conn)?;
            if !decision_open(&tx, &lineage_id, expected_generation, &failed_instance_id)? {
                return Ok(false);
            }
            insert_decision(
                &tx,
                &failed_instance_id,
                &lineage_id,
                expected_generation,
                cause,
                outcome::EXPIRED,
                None,
                Some(&reason),
            )?;
            tx.commit()?;
            Ok(true)
        })
        .await
    }

    /// Restart decisions of every cause for `lineage_id` in the last hour.
    async fn restarts_in_last_hour(&self, lineage_id: String) -> Result<u32, StoreError> {
        self.read("restarts_in_last_hour", move |conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM lineage_restart_decisions
                  WHERE lineage_id = ?1 AND outcome = ?2 AND decided_at >= ?3",
                params![
                    lineage_id,
                    outcome::RESTARTED,
                    crate::config::rfc3339_minutes_ago(60)
                ],
                |row| row.get(0),
            )?;
            Ok(u32::try_from(count).unwrap_or(u32::MAX))
        })
        .await
    }

    /// F without successor for a decision (`restart-cap` / `process-exit`),
    /// with the decision row, in one writer transaction.
    async fn decide_pause(
        &self,
        spec: FenceSpec,
        failed_instance_id: String,
        cause: &'static str,
        decision_outcome: &'static str,
        reason: Option<String>,
        push: Option<&'static str>,
    ) -> Result<(FenceTx, Settlement), StoreError> {
        self.run_named("restart_decide_pause", move |conn| {
            let tx = immediate_tx(conn)?;
            if !decision_open(
                &tx,
                &spec.lineage_id,
                spec.expected_generation,
                &failed_instance_id,
            )? {
                return Ok((FenceTx::CasLost, Settlement::default()));
            }
            let result = fence_core(&tx, &spec)?;
            let FenceTx::Committed(core) = &result else {
                return Ok((result, Settlement::default()));
            };
            insert_decision(
                &tx,
                &failed_instance_id,
                &spec.lineage_id,
                spec.expected_generation,
                cause,
                decision_outcome,
                Some(&core.fence_id),
                reason.as_deref(),
            )?;
            if let Some(kind) = push {
                queue_push(
                    &tx,
                    &core.fence_id,
                    &spec.lineage_id,
                    &failed_instance_id,
                    kind,
                    Some(cause),
                )?;
            }
            let settlement = core.settlement.clone();
            tx.commit()?;
            Ok((result, settlement))
        })
        .await
    }
}

/// How often the tick refreshes the Hub run marker.
const ALIVE_EVERY: std::time::Duration = std::time::Duration::from_secs(15);

impl Store {
    async fn write_alive(&self) -> Result<(), StoreError> {
        self.run_named("hub_note_alive", |conn| {
            conn.execute(
                "INSERT INTO hub_run_marker (id, last_alive_at) VALUES (1, ?1)
                 ON CONFLICT(id) DO UPDATE SET last_alive_at = excluded.last_alive_at",
                params![crate::config::now_rfc3339()],
            )?;
            Ok(())
        })
        .await
    }
}

/// Scheduler tick, whatever the supervisor setting: refresh the persisted
/// "this Hub was alive at" marker (at most every [`ALIVE_EVERY`]). The next
/// boot reads it to tell an end seen moments before the Hub stopped from
/// history (§6.3.2).
pub(crate) async fn note_alive(state: &AppState) {
    {
        let mut last = state
            .restart
            .alive_written
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if last.is_some_and(|at| at.elapsed() < ALIVE_EVERY) {
            return;
        }
        *last = Some(std::time::Instant::now());
    }
    if let Err(error) = state.store.write_alive().await {
        tracing::warn!(%error, "could not record the Hub run marker");
    }
}

/// Scheduler tick: one evaluation unless the previous one is still running.
pub(crate) async fn tick(state: &AppState) {
    let Ok(_guard) = state.restart.running.clone().try_lock_owned() else {
        return;
    };
    evaluate(state).await;
}

/// Boot: decide every failure left pending by the previous Hub process.
pub(crate) async fn boot(state: &AppState) {
    let _guard = state.restart.running.lock().await;
    let decided = evaluate(state).await;
    if !decided.is_empty() {
        tracing::info!(
            count = decided.len(),
            "boot evaluation decided pending C1 failures"
        );
    }
}

/// One evaluation of every candidate lineage. Unguarded: concurrent calls are
/// safe (the CAS and the key decide at most once) and the C1 suites run them
/// concurrently on purpose.
pub(crate) async fn evaluate(state: &AppState) -> Vec<Decided> {
    // §6.4 bookkeeping first: a notice orphaned by a rejected successor
    // create settles with that reason before any new F could cancel it as
    // fenced, and notices due since the last tick go out.
    drive_notices(state).await;
    let candidates = match state.store.restart_candidates().await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "C1 supervisor could not read candidates");
            return Vec::new();
        }
    };
    let mut decided = Vec::new();
    let window_start = crate::config::rfc3339_minutes_ago(DECISION_WINDOW_MINUTES);
    for (lineage_id, failed_id, seen) in candidates {
        let pending = match pending(state, &lineage_id, &failed_id, &seen, &window_start).await {
            Ok(Some(pending)) => pending,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(%error, %lineage_id, "C1 supervisor could not read a candidate");
                continue;
            }
        };
        match decide(state, pending).await {
            Ok(Some(decision)) => {
                tracing::info!(
                    lineage_id = %decision.lineage_id,
                    failed = %decision.failed_instance_id,
                    cause = %decision.cause,
                    outcome = %decision.outcome,
                    successor = ?decision.successor_instance_id,
                    "C1 restart decision"
                );
                decided.push(decision);
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, %lineage_id, "C1 restart decision failed");
            }
        }
    }
    // §6.4: notices whose successor is ready (also after a Hub restart).
    drive_notices(state).await;
    // §8.5: C1 pushes recorded by an F and not sent yet.
    drive_pushes(state).await;
    decided
}

/// Load and check one candidate: Node-attested process-end evidence on the
/// current chapter at the live generation.
async fn pending(
    state: &AppState,
    lineage_id: &str,
    failed_id: &str,
    seen: &str,
    window_start: &str,
) -> Result<Option<Pending>, StoreError> {
    let Some(lineage) = state.store.get_lineage(lineage_id.to_owned()).await? else {
        return Ok(None);
    };
    let Some(failed) = state.store.get_instance_read(failed_id.to_owned()).await? else {
        return Ok(None);
    };
    if lineage.current_instance_id != failed.instance_id
        || lineage.generation != failed.generation
        || lineage.state == "paused"
        || failed.fenced_at.is_some()
        || !state
            .store
            .instance_has_process_end_evidence(&failed)
            .await?
    {
        return Ok(None);
    }
    let cause = classify(
        &lineage.state,
        &failed.lifecycle,
        failed.last_error.as_deref(),
    );
    Ok(Some(Pending {
        lineage,
        failed,
        cause,
        // RFC3339 stamps of one fixed-width shape compare chronologically.
        expired_seen: (seen < window_start).then(|| seen.to_owned()),
    }))
}

/// Decide one pending failure. `None` when another F got there first.
async fn decide(
    state: &AppState,
    pending: Pending,
) -> Result<Option<Decided>, crate::error::HubError> {
    let Pending {
        lineage,
        failed,
        cause,
        expired_seen,
    } = pending;
    if let Some(seen) = expired_seen {
        let reason = format!(
            "the Hub saw this end at {seen}, more than {DECISION_WINDOW_MINUTES} minutes before \
             the supervisor evaluated it; not relaunched (Resume continues it)"
        );
        let recorded = state
            .store
            .decide_expired(
                lineage.lineage_id.clone(),
                lineage.generation,
                failed.instance_id.clone(),
                cause,
                reason,
            )
            .await?;
        return Ok(recorded.then(|| Decided {
            lineage_id: lineage.lineage_id.clone(),
            failed_instance_id: failed.instance_id.clone(),
            cause: cause.into(),
            outcome: outcome::EXPIRED.into(),
            successor_instance_id: None,
        }));
    }
    // Deterministic race hook: after the evidence read, before any writer
    // job (a close or Resume parked here commits first). No-op in production.
    crate::store::run_continuation_hook("restart-read", &lineage.lineage_id);
    let policy: Option<RestartPolicy> = lineage
        .restart
        .clone()
        .and_then(|value| serde_json::from_value(value).ok());
    let Some(policy) = policy.filter(|policy| policy.on_process_loss) else {
        // Without `restart.onProcessLoss` the end keeps today's exited push.
        return pause(
            state,
            &lineage,
            &failed,
            cause,
            outcome::PROCESS_EXIT,
            None,
            None,
        )
        .await;
    };
    let restarts = state
        .store
        .restarts_in_last_hour(lineage.lineage_id.clone())
        .await?;
    if restarts >= policy.max_per_hour {
        return pause(
            state,
            &lineage,
            &failed,
            cause,
            outcome::RESTART_CAP,
            None,
            Some(push_kind::RESTART_CAP),
        )
        .await;
    }
    restart(state, &lineage, &failed, cause).await
}

/// F without successor: `paused{restart-cap}` or `paused{process-exit}`.
async fn pause(
    state: &AppState,
    lineage: &LineageRecord,
    failed: &InstanceRecord,
    cause: &'static str,
    decision_outcome: &'static str,
    reason: Option<String>,
    push: Option<&'static str>,
) -> Result<Option<Decided>, crate::error::HubError> {
    let paused_by =
        json!({"kind": decision_outcome, "instanceId": failed.instance_id, "cause": cause});
    let spec = FenceSpec {
        lineage_id: lineage.lineage_id.clone(),
        expected_generation: lineage.generation,
        from_states: &["starting", "running"],
        mode: FenceMode::Pause {
            paused_by,
            // The process has already ended: nothing to close.
            close: None,
        },
        actor: json!({"by": SUPERVISOR, "cause": cause}),
        authority: None,
        self_close_guard: false,
    };
    let result = state
        .publish_settlement(async {
            state
                .store
                .decide_pause(
                    spec,
                    failed.instance_id.clone(),
                    cause,
                    decision_outcome,
                    reason,
                    push,
                )
                .await
        })
        .await
        .map_err(crate::http::map_store)?;
    let FenceTx::Committed(core) = result else {
        return Ok(None);
    };
    crate::fence::after_commit(state, &core, None).await;
    Ok(Some(Decided {
        lineage_id: lineage.lineage_id.clone(),
        failed_instance_id: failed.instance_id.clone(),
        cause: cause.into(),
        outcome: decision_outcome.into(),
        successor_instance_id: None,
    }))
}

/// OA5: the origin the successor's create carries — the seating device's
/// own origin, recorded at create as `seatedOrigin`. Never the create spec's
/// `origin` key, which is incidental to how the spec was stamped. A lineage
/// seated before the record existed is Human-seated by construction: only a
/// Human device may set `restart` (403 otherwise, §6.1), and the supervisor
/// relaunches only lineages with `restart.onProcessLoss`.
fn seated_origin(origin_spec: Option<&Value>) -> String {
    origin_spec
        .and_then(|reference| reference.get("seatedOrigin"))
        .and_then(Value::as_str)
        .unwrap_or("human")
        .to_owned()
}

/// F with successor: the restart.
async fn restart(
    state: &AppState,
    lineage: &LineageRecord,
    failed: &InstanceRecord,
    cause: &'static str,
) -> Result<Option<Decided>, crate::error::HubError> {
    let Some(host) = state.store.get_host(failed.host_id.clone()).await? else {
        return Ok(None);
    };
    let recovery = crate::http::recovery_source(state, lineage, failed).await?;
    // §6.3.1: the successor always runs claude-sdk on the same host (a
    // non-Claude kind cannot, so it keeps its own carrier).
    let driver = if failed.kind == "claude" {
        "claude-sdk"
    } else {
        failed.driver.as_str()
    };
    #[cfg(any(test, feature = "test-faults"))]
    let injected = state.restart.take_prepare_fault();
    #[cfg(not(any(test, feature = "test-faults")))]
    let injected: Option<crate::error::HubError> = None;
    let prepared = match match injected {
        Some(error) => Err(crate::http::ContinuationError::Other(error)),
        None => {
            crate::http::prepare_continuation(state, host, lineage, failed, &recovery, driver, None)
                .await
        }
    } {
        Ok(prepared) => prepared,
        Err(crate::http::ContinuationError::Gone(what)) => {
            // What the successor needs no longer exists (its provider profile
            // or via route): retrying cannot help. The chapter's process
            // ended and the Hub cannot continue it: `paused{process-exit}`,
            // the reason on the decision, and the "could not restart" push
            // (the generic exited push is replaced for this lineage).
            let reason = format!("successor not prepared: {what}");
            return pause(
                state,
                lineage,
                failed,
                cause,
                outcome::PROCESS_EXIT,
                Some(reason),
                Some(push_kind::RESTART_FAILED),
            )
            .await;
        }
        Err(crate::http::ContinuationError::Other(error)) => {
            // Possibly transient (a store or lookup error, a via host
            // offline): decide nothing, so the next tick retries. Logged on
            // the first failure of the chapter and then once a minute.
            if state.restart.note_prepare_failure(&failed.instance_id) {
                tracing::warn!(%error, failed = %failed.instance_id,
                    "C1 successor could not be prepared; retrying on the next tick");
            }
            return Ok(None);
        }
    };
    let origin_spec = lineage.origin_spec_ref.as_ref();
    let origin = seated_origin(origin_spec);
    let seated_by = origin_spec
        .and_then(|reference| reference.get("deviceId"))
        .cloned()
        .unwrap_or(Value::Null);
    let decision = RestartDecision {
        failed_instance_id: failed.instance_id.clone(),
        cause,
        recovery: recovery.as_str(),
        recovery_session_id: recovery.session().map(str::to_owned),
    };
    let result = state
        .store
        .continuation_resume(crate::store::ContinuationResumeRequest {
            addressed_instance_id: failed.instance_id.clone(),
            expected_generation: lineage.generation,
            host_id: failed.host_id.clone(),
            spec: prepared.spec.clone(),
            operation: prepared.operation.to_string(),
            prompt: None,
            origin,
            title: failed.title.clone(),
            driver: prepared.driver.clone(),
            actor: json!({"by": SUPERVISOR, "cause": cause, "seatedBy": seated_by}),
            chapter_cause: cause.into(),
            from_states: &["starting", "running"],
            restart: Some(decision),
        })
        .await
        .map_err(crate::http::map_store)?;
    let resumed = match result {
        crate::store::ContinuationResumeResult::Resumed(resumed) => resumed,
        crate::store::ContinuationResumeResult::Superseded { .. } => return Ok(None),
    };
    if let Err(error) = crate::http::finish_continuation(state, &resumed, &prepared.spec).await {
        // The decision committed; the create stays queued and the fence
        // delivery (hello or tick) forwards it.
        tracing::warn!(%error, successor = %resumed.successor.instance_id,
            "forwarding a C1 successor's create failed; it stays queued");
    }
    Ok(Some(Decided {
        lineage_id: lineage.lineage_id.clone(),
        failed_instance_id: failed.instance_id.clone(),
        cause: cause.into(),
        outcome: outcome::RESTARTED.into(),
        successor_instance_id: Some(resumed.successor.instance_id.clone()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_successor_origin_is_the_seating_origin_never_the_spec_origin() {
        let spec = json!({"origin": "agent", "seatedOrigin": "human", "spec": {"origin": "agent"}});
        assert_eq!(seated_origin(Some(&spec)), "human");
        // Seated before the record existed: Human by construction.
        assert_eq!(seated_origin(Some(&json!({"origin": "agent"}))), "human");
        assert_eq!(seated_origin(None), "human");
    }

    #[test]
    fn the_cause_table_classifies_each_evidence_row() {
        // Row 1: ended by itself after reporting running.
        assert_eq!(classify("running", "exited", None), cause::PROCESS_EXIT);
        assert_eq!(
            classify("running", "failed", Some("signal KILL")),
            cause::PROCESS_EXIT
        );
        // Row 2: never reported running, or an attested launch failure.
        assert_eq!(classify("starting", "exited", None), cause::START_FAILED);
        assert_eq!(
            classify("running", "failed", Some("create-never-acknowledged")),
            cause::START_FAILED
        );
        assert_eq!(
            classify("running", "failed", Some("native-driver-start-failed")),
            cause::START_FAILED
        );
        // Row 3: the Node restarted without the chapter, whatever its state.
        for state in ["starting", "running"] {
            assert_eq!(
                classify(state, "exited", Some("node-epoch-changed")),
                cause::NODE_RESTARTED
            );
        }
    }
}
