//! Delivery of inbox facts to their subscriber lineage (cb-inbox-deliver;
//! D-057 §10, D-060 §10.1 items 4–7 and 9).
//!
//! `inbox.rs` writes the fact rows; this module hands them to the lineage's
//! live chapter as Hub-voiced batches, on the writer thread:
//!
//! * **The gate (§10, Delivery).** A batch forms for a lineage only when its
//!   live chapter is idle (the Phase 1 projection), has no pending
//!   interaction of its own, and has no owner-origin command queued or in
//!   flight. Owner messages always go first; a busy chapter's batch waits and
//!   is never rejected. The gate is evaluated when a state change can open
//!   it — a new fact row, the chapter turning idle or live, an interaction
//!   leaving `pending`, a command settling, a lineage state change (pause,
//!   resume, a successor), its host coming back — each queued by a trigger in
//!   the writing transaction (`inbox_deliver_due`) and drained after the
//!   writer job. Nothing polls a Node.
//! * **Coalescing.** Every undelivered row goes into one batch, one
//!   `instance.send` with origin `hub`.
//! * **Re-check (§10.1 item 4).** At formation each row's subscriber must
//!   still be its subject's parent and must not have muted its kind. A row
//!   that fails falls back to the owner push where its kind pages today
//!   (see [`paging_alert`]) and is neither held nor recorded as consumed.
//! * **Ids (§10, D-055).** The batch's command id is chosen and its command
//!   row persisted when it forms, with the Hub-internal initiator for the
//!   live chapter's generation. The forward is the ordinary
//!   `forward_if_online`, so a same-id forward of a row already forwarded
//!   writes nothing, an unknown outcome stays on that id until the Node's
//!   journal settles it, and nothing is ever re-sent under a new id.
//! * **Fencing.** F cancels an unforwarded batch (§7.2 step 4); a trigger
//!   hands its rows back, and they form a new batch, under a new id, for the
//!   successor. A batch whose send may have reached the chapter keeps its
//!   rows: delivery is at most once per batch.
//! * **Paused lineages hold (§10.1 item 5).** No live chapter, no batch, and
//!   no retry: the next lineage state change (Resume, a successor) re-opens
//!   the gate.
//! * **Visible row first (§10.1 item 6).** The batch row (`inbox_batches`,
//!   with the verbatim text, the opening tag, the template version and the
//!   fact ids) commits in the same transaction as the command row, before
//!   any forward intent.
//! * **Logs (§10.1 item 9).** Kinds, counts and error codes; never fact text,
//!   worker output or ids.

use crate::AppState;
use crate::config::now_rfc3339;
use crate::inbox_voice::{self as voice, Line, Snapshot, untrusted};
use crate::store::{StoreError, ensure_column};
use remuda_protocol::{InboxBatch, InboxFactKind, InboxFallbackReason};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

/// Journal rows read from a subject's tail for its last assistant text.
const TAIL_EVENTS: i64 = 64;

/// The predicate every due trigger shares: the lineage has rows waiting.
macro_rules! waiting_rows {
    ($lineage:expr) => {
        concat!(
            "EXISTS (SELECT 1 FROM inbox_facts f WHERE f.subscriber_lineage_id = ",
            $lineage,
            " AND f.batch_id IS NULL AND f.fallback_at IS NULL)"
        )
    };
}

/// The predicate for a lineage with a formed batch still queued and not
/// forwarded: one that waits behind input that went first, or was released
/// for a same-id retry.
macro_rules! unsent_batch {
    ($lineage:expr) => {
        concat!(
            "EXISTS (SELECT 1 FROM inbox_batches b JOIN commands c ON c.id = b.batch_id
                      WHERE b.lineage_id = ",
            $lineage,
            " AND b.outcome IS NULL AND c.state = 'queued' AND c.forwarded = 0)"
        )
    };
}

/// Idempotent DDL; no `user_version` rung. The tables live in `hub.sqlite`,
/// which `scripts/hub-backup.sh` carries whole. The triggers are rebuilt on
/// every open, so a change to their predicates reaches existing databases.
pub(crate) fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS inbox_batches (
            batch_id TEXT PRIMARY KEY,
            lineage_id TEXT NOT NULL,
            instance_id TEXT NOT NULL,
            generation INTEGER NOT NULL,
            host_id TEXT NOT NULL,
            opening_tag TEXT NOT NULL,
            template_version INTEGER NOT NULL,
            fact_ids_json TEXT NOT NULL,
            listed INTEGER NOT NULL,
            text TEXT NOT NULL,
            created_at TEXT NOT NULL,
            outcome TEXT,
            settled_at TEXT,
            released_at TEXT
         );
         CREATE INDEX IF NOT EXISTS inbox_batches_open
            ON inbox_batches(instance_id) WHERE outcome IS NULL;
         CREATE TABLE IF NOT EXISTS inbox_deliver_due (
            lineage_id TEXT PRIMARY KEY
         );
         CREATE TABLE IF NOT EXISTS inbox_fallback_pushes (
            fact_id INTEGER PRIMARY KEY,
            alert TEXT NOT NULL,
            instance_id TEXT NOT NULL,
            interaction_id TEXT,
            created_at TEXT NOT NULL,
            sent_at TEXT,
            recipients INTEGER
         );",
    )?;
    ensure_column(conn, "inbox_facts", "fallback_at", "TEXT")?;
    ensure_column(conn, "inbox_facts", "fallback_reason", "TEXT")?;
    conn.execute_batch(concat!(
        "DROP TRIGGER IF EXISTS inbox_due_fact;
         CREATE TRIGGER inbox_due_fact AFTER INSERT ON inbox_facts
         BEGIN
           INSERT OR IGNORE INTO inbox_deliver_due (lineage_id)
           VALUES (NEW.subscriber_lineage_id);
         END;
         DROP TRIGGER IF EXISTS inbox_due_idle;
         CREATE TRIGGER inbox_due_idle AFTER UPDATE OF activity, lifecycle ON instances
         WHEN NEW.activity = 'idle' AND NEW.lifecycle IN ('ready', 'running')
          AND (OLD.activity IS NOT 'idle' OR OLD.lifecycle NOT IN ('ready', 'running'))
          AND ",
        waiting_rows!("COALESCE(NEW.lineage_id, NEW.id)"),
        "
         BEGIN
           INSERT OR IGNORE INTO inbox_deliver_due (lineage_id)
           VALUES (COALESCE(NEW.lineage_id, NEW.id));
         END;
         -- A lineage without continuity whose one instance ended or was
         -- deleted: its waiting rows have no subscriber left.
         DROP TRIGGER IF EXISTS inbox_due_ended;
         CREATE TRIGGER inbox_due_ended AFTER UPDATE OF lifecycle, ended_at ON instances
         WHEN ((NEW.ended_at IS NOT NULL AND OLD.ended_at IS NULL)
               OR (NEW.lifecycle IN ('exited', 'failed', 'closed')
                   AND OLD.lifecycle NOT IN ('exited', 'failed', 'closed')))
          AND NOT EXISTS (SELECT 1 FROM lineages
                           WHERE lineage_id = COALESCE(NEW.lineage_id, NEW.id))
          AND ",
        waiting_rows!("COALESCE(NEW.lineage_id, NEW.id)"),
        "
         BEGIN
           INSERT OR IGNORE INTO inbox_deliver_due (lineage_id)
           VALUES (COALESCE(NEW.lineage_id, NEW.id));
         END;
         DROP TRIGGER IF EXISTS inbox_due_deleted;
         CREATE TRIGGER inbox_due_deleted AFTER DELETE ON instances
         WHEN NOT EXISTS (SELECT 1 FROM lineages
                           WHERE lineage_id = COALESCE(OLD.lineage_id, OLD.id))
          AND ",
        waiting_rows!("COALESCE(OLD.lineage_id, OLD.id)"),
        "
         BEGIN
           INSERT OR IGNORE INTO inbox_deliver_due (lineage_id)
           VALUES (COALESCE(OLD.lineage_id, OLD.id));
         END;
         DROP TRIGGER IF EXISTS inbox_due_interaction;
         CREATE TRIGGER inbox_due_interaction AFTER UPDATE OF state ON interactions
         WHEN OLD.state = 'pending' AND NEW.state <> 'pending'
         BEGIN
           INSERT OR IGNORE INTO inbox_deliver_due (lineage_id)
           SELECT COALESCE(i.lineage_id, i.id) FROM instances i
            WHERE i.id = NEW.instance_id AND ",
        waiting_rows!("COALESCE(i.lineage_id, i.id)"),
        ";
         END;
         DROP TRIGGER IF EXISTS inbox_due_interaction_gone;
         CREATE TRIGGER inbox_due_interaction_gone AFTER DELETE ON interactions
         WHEN OLD.state = 'pending'
         BEGIN
           INSERT OR IGNORE INTO inbox_deliver_due (lineage_id)
           SELECT COALESCE(i.lineage_id, i.id) FROM instances i
            WHERE i.id = OLD.instance_id AND ",
        waiting_rows!("COALESCE(i.lineage_id, i.id)"),
        ";
         END;
         -- A command settling: the owner's input has gone first, or a batch
         -- ended. A batch whose send provably never reached its chapter
         -- (never forwarded, withheld at the send gate, refused or cancelled
         -- as fenced, or settled by its Node as never dispatched) hands its
         -- rows back for a new batch under a new id; any other outcome keeps
         -- them (at most once per batch). A refusal before the frame left
         -- the Hub (NODE_BUSY, the link gone) or before the Node recorded it
         -- never settles the batch: it is released back to the queue and the
         -- same id is retried (`drive`).
         DROP TRIGGER IF EXISTS inbox_due_command;
         CREATE TRIGGER inbox_due_command AFTER UPDATE OF state ON commands
         WHEN NEW.state = 'settled' AND OLD.state <> 'settled' AND NEW.instance_id IS NOT NULL
         BEGIN
           UPDATE inbox_batches
              SET outcome = COALESCE(NEW.settlement_outcome, 'completed'),
                  settled_at = NEW.updated_at,
                  released_at = CASE
                    WHEN NEW.forwarded = 0 OR NEW.settlement_outcome = 'withheld'
                      OR NEW.settlement_reason = 'fenced'
                      OR NEW.settlement_not_dispatched = 1 THEN NEW.updated_at END
            WHERE batch_id = NEW.id AND outcome IS NULL;
           UPDATE inbox_facts SET batch_id = NULL
            WHERE batch_id = NEW.id
              AND (NEW.forwarded = 0 OR NEW.settlement_outcome = 'withheld'
                   OR NEW.settlement_reason = 'fenced'
                   OR NEW.settlement_not_dispatched = 1);
           -- Also a lineage whose formed batch waits in the queue behind
           -- the input that just settled.
           INSERT OR IGNORE INTO inbox_deliver_due (lineage_id)
           SELECT COALESCE(i.lineage_id, i.id) FROM instances i
            WHERE i.id = NEW.instance_id AND (",
        waiting_rows!("COALESCE(i.lineage_id, i.id)"),
        " OR ",
        unsent_batch!("COALESCE(i.lineage_id, i.id)"),
        ");
         END;
         DROP TRIGGER IF EXISTS inbox_due_lineage;
         CREATE TRIGGER inbox_due_lineage AFTER UPDATE ON lineages
         WHEN (NEW.state IS NOT OLD.state
               OR NEW.current_instance_id IS NOT OLD.current_instance_id
               OR NEW.generation IS NOT OLD.generation)
          AND ",
        waiting_rows!("NEW.lineage_id"),
        "
         BEGIN
           INSERT OR IGNORE INTO inbox_deliver_due (lineage_id) VALUES (NEW.lineage_id);
         END;
         DROP TRIGGER IF EXISTS inbox_due_host;
         CREATE TRIGGER inbox_due_host AFTER UPDATE OF state ON hosts
         WHEN NEW.state = 'online' AND OLD.state IS NOT 'online'
         BEGIN
           INSERT OR IGNORE INTO inbox_deliver_due (lineage_id)
           SELECT DISTINCT f.subscriber_lineage_id FROM inbox_facts f
            WHERE f.batch_id IS NULL AND f.fallback_at IS NULL
              AND EXISTS (SELECT 1 FROM instances i
                           WHERE i.host_id = NEW.id AND i.ended_at IS NULL
                             AND COALESCE(i.lineage_id, i.id) = f.subscriber_lineage_id);
         END;"
    ))
}

/// At open: every lineage with rows waiting is due, so a row the previous
/// process left (or released) is batched once its gate opens.
pub(crate) fn mark_all_due(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO inbox_deliver_due (lineage_id)
         SELECT DISTINCT subscriber_lineage_id FROM inbox_facts
          WHERE batch_id IS NULL AND fallback_at IS NULL",
        [],
    )?;
    Ok(())
}

// ── the gate ───────────────────────────────────────────────────────────────

/// The live chapter a batch targets.
#[derive(Debug, Clone)]
struct Chapter {
    instance_id: String,
    generation: i64,
    host_id: String,
}

/// Why a lineage's rows wait. Logged as a category only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hold {
    /// No live chapter: paused, starting, or its chapter ended.
    NoLiveChapter,
    Paused,
    Busy,
    Interaction,
    /// An owner-origin input (a send, an answer, keys, terminal bytes) is
    /// queued or in flight: it goes first. The chapter's own launch and the
    /// owner's control verbs are not input and hold nothing.
    OwnerFirst,
    /// Hub-written input to the chapter has not settled: its previous batch,
    /// or a successor's restart notice, which stays its first input (§6.4).
    InFlight,
    HostOffline,
    /// Hub-origin input goes only to Nodes that advertise the fence
    /// capability (D-057 §6.4).
    HostLacksFence,
    /// Not a hold: the lineage has no continuity and its one instance ended
    /// or is gone, so no chapter will ever take a batch. Its rows have no
    /// subscriber and fall back to the owner push.
    Gone,
}

impl Hold {
    fn label(self) -> &'static str {
        match self {
            Self::NoLiveChapter => "no-live-chapter",
            Self::Paused => "paused",
            Self::Busy => "busy",
            Self::Interaction => "pending-interaction",
            Self::OwnerFirst => "owner-first",
            Self::InFlight => "in-flight",
            Self::HostOffline => "host-offline",
            Self::HostLacksFence => "host-lacks-fence",
            Self::Gone => "gone",
        }
    }
}

/// The lineage's live chapter when all three delivery conditions hold
/// (§10, Delivery), else why its rows wait.
fn gate(conn: &Connection, lineage_id: &str) -> Result<Result<Chapter, Hold>, StoreError> {
    let lineage: Option<(String, i64, String)> = conn
        .query_row(
            "SELECT current_instance_id, generation, state FROM lineages WHERE lineage_id = ?1",
            params![lineage_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    // A continuity lineage delivers to its current chapter while it runs; a
    // lineage without continuity is the one instance whose id it is.
    let (chapter_id, live_generation) = match lineage {
        Some((_, _, state)) if state == "paused" => return Ok(Err(Hold::Paused)),
        Some((_, _, state)) if state != "running" => return Ok(Err(Hold::NoLiveChapter)),
        Some((current, generation, _)) => (current, Some(generation)),
        None => (lineage_id.to_owned(), None),
    };
    #[allow(clippy::type_complexity)]
    let row: Option<(
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        i64,
        String,
    )> = conn
        .query_row(
            "SELECT host_id, lifecycle, activity, fenced_at, ended_at, generation,
                    COALESCE(lineage_id, id)
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
                    row.get(6)?,
                ))
            },
        )
        .optional()?;
    let ended = |lifecycle: &str, ended_at: &Option<String>| {
        ended_at.is_some() || matches!(lifecycle, "exited" | "failed" | "closed")
    };
    let Some((host_id, lifecycle, activity, fenced_at, ended_at, generation, own)) = row else {
        // A lineage without continuity whose one instance is gone has no
        // subscriber left; a continuity lineage keeps its rows for Resume.
        return Ok(Err(if live_generation.is_none() {
            Hold::Gone
        } else {
            Hold::NoLiveChapter
        }));
    };
    if live_generation.is_none() && own == lineage_id && ended(&lifecycle, &ended_at) {
        return Ok(Err(Hold::Gone));
    }
    if own != lineage_id
        || fenced_at.is_some()
        || ended_at.is_some()
        || !matches!(lifecycle.as_str(), "ready" | "running")
        || live_generation.is_some_and(|live| live != generation)
    {
        return Ok(Err(Hold::NoLiveChapter));
    }
    if activity != "idle" {
        return Ok(Err(Hold::Busy));
    }
    let interaction: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM interactions WHERE instance_id = ?1 AND state = 'pending')",
        params![chapter_id],
        |row| row.get(0),
    )?;
    if interaction {
        return Ok(Err(Hold::Interaction));
    }
    let in_flight: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM inbox_batches WHERE instance_id = ?1 AND outcome IS NULL)",
        params![chapter_id],
        |row| row.get(0),
    )?;
    if let Some(hold) = input_ahead(conn, &chapter_id, None)? {
        return Ok(Err(hold));
    }
    if in_flight {
        return Ok(Err(Hold::InFlight));
    }
    let online: bool = conn
        .query_row(
            "SELECT state = 'online' FROM hosts WHERE id = ?1",
            params![host_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(false);
    if !online {
        return Ok(Err(Hold::HostOffline));
    }
    if !crate::fence::host_capable(conn, &host_id)? {
        return Ok(Err(Hold::HostLacksFence));
    }
    Ok(Ok(Chapter {
        instance_id: chapter_id,
        generation,
        host_id,
    }))
}

/// Input that goes before a batch to `chapter`: owner input queued or in
/// flight ([`Hold::OwnerFirst`]), or Hub-written input not yet settled (a
/// previous batch, a successor's restart notice; [`Hold::InFlight`]).
/// `except` leaves out the batch being forwarded itself.
fn input_ahead(
    conn: &Connection,
    chapter: &str,
    except: Option<&str>,
) -> Result<Option<Hold>, StoreError> {
    let (owner, hub): (bool, bool) = conn.query_row(
        "SELECT
            EXISTS (SELECT 1 FROM commands
                     WHERE instance_id = ?1 AND state <> 'settled'
                       AND operation IN ('instance.send', 'instance.respond',
                                         'instance.keys', 'tty.write')
                       AND initiator_instance_id IS NULL
                       AND json_extract(payload_json, '$.origin') = 'human'),
            EXISTS (SELECT 1 FROM commands
                     WHERE instance_id = ?1 AND state <> 'settled'
                       AND operation = 'instance.send'
                       AND json_extract(payload_json, '$.origin') = 'hub'
                       AND id IS NOT ?2)",
        params![chapter, except],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(if owner {
        Some(Hold::OwnerFirst)
    } else if hub {
        Some(Hold::InFlight)
    } else {
        None
    })
}

/// Whether the batch `command_id` must wait, at the moment its forward
/// intent would commit: owner input (or other Hub input) to its chapter
/// arrived after the batch formed and goes first. Run inside the writer job
/// that marks the intent, so nothing can slip between this check and the
/// mark. `false` for any command that is not a batch.
pub(crate) fn forward_waits(conn: &Connection, command_id: &str) -> Result<bool, StoreError> {
    let chapter: Option<String> = conn
        .query_row(
            "SELECT instance_id FROM inbox_batches WHERE batch_id = ?1",
            params![command_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(chapter) = chapter else {
        return Ok(false);
    };
    Ok(input_ahead(conn, &chapter, Some(command_id))?.is_some())
}

// ── the re-check and the owner-push fallback ──────────────────────────────

/// One waiting row.
#[derive(Debug, Clone)]
struct Waiting {
    id: i64,
    kind: InboxFactKind,
    subject: Option<String>,
    run: i64,
    entered_at: String,
    payload: Value,
}

fn waiting(conn: &Connection, lineage_id: &str) -> Result<Vec<Waiting>, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, kind, subject_instance_id, run_generation, state_entered_at, payload_json
           FROM inbox_facts
          WHERE subscriber_lineage_id = ?1 AND batch_id IS NULL AND fallback_at IS NULL
          ORDER BY id",
    )?;
    let rows = stmt
        .query_map(params![lineage_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        // A kind this build does not know stays where it is.
        .filter_map(|(id, kind, subject, run, entered_at, payload)| {
            Some(Waiting {
                id,
                kind: InboxFactKind::parse(&kind)?,
                subject,
                run,
                entered_at,
                payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
            })
        })
        .collect())
}

/// Kinds whose subscriber is the subject's parent lineage (`subscriber_of`).
/// A gate fact goes to the job's initiator lineage, a host fact to every
/// lineage whose scope covers the host, and the wake kinds to the lineage
/// that wrote them: no parent edge to re-check.
fn parent_routed(kind: InboxFactKind) -> bool {
    !matches!(
        kind,
        InboxFactKind::Gate
            | InboxFactKind::HostOnline
            | InboxFactKind::HostOffline
            | InboxFactKind::WakeDue
            | InboxFactKind::ContextThreshold
    )
}

/// §10.1 item 4, at batch formation: `None` when the row still belongs in
/// `lineage_id`'s batch.
fn recheck(
    conn: &Connection,
    lineage_id: &str,
    muted: &[InboxFactKind],
    row: &Waiting,
) -> Result<Option<InboxFallbackReason>, StoreError> {
    if muted.contains(&row.kind) {
        return Ok(Some(InboxFallbackReason::Muted));
    }
    if !parent_routed(row.kind) {
        return Ok(None);
    }
    let Some(subject) = row.subject.as_deref() else {
        return Ok(Some(InboxFallbackReason::NoSubscriber));
    };
    Ok(match crate::inbox::subscriber_of(conn, subject)? {
        None => Some(InboxFallbackReason::NoSubscriber),
        Some(current) if current != lineage_id => Some(InboxFallbackReason::Reparented),
        Some(_) => None,
    })
}

/// The owner alert a fallen-back row pages, where its kind pages today:
///
/// * `interaction`: "Need your input" (`alerts.rs` pages every interaction
///   request), while the interaction is still pending;
/// * `exited`: "Session exited", unless the lineage restarts on process loss
///   (its C1 push replaces that one, §8.5);
/// * `waiting`: "Still waiting" (the blocked reminder), while the subject is
///   still blocked.
///
/// No other kind pages today: `alerts.rs` pages a turn error only for the
/// legacy `turn_done` event with `isError`, which no current carrier
/// journals, so a structured `turn-failed` row has no owner push to fall
/// back to; gate, host, supply, stall, budget and wake facts never paged.
fn paging_alert(
    conn: &Connection,
    row: &Waiting,
) -> Result<Option<(&'static str, Option<String>)>, StoreError> {
    let Some(subject) = row.subject.as_deref() else {
        return Ok(None);
    };
    match row.kind {
        InboxFactKind::Interaction => {
            let Some(interaction) = row.payload.get("interactionId").and_then(Value::as_str) else {
                return Ok(None);
            };
            let pending: bool = conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM interactions WHERE id = ?1 AND state = 'pending')",
                params![interaction],
                |row| row.get(0),
            )?;
            Ok(pending.then(|| ("interaction", Some(interaction.to_owned()))))
        }
        InboxFactKind::Exited => {
            let restarts: bool = conn
                .query_row(
                    "SELECT COALESCE(json_extract(restart_json, '$.onProcessLoss'), 0)
                       FROM instances WHERE id = ?1",
                    params![subject],
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(false);
            Ok((!restarts).then_some(("exited", None)))
        }
        InboxFactKind::Waiting => {
            let blocked: bool = conn
                .query_row(
                    "SELECT activity = 'blocked' FROM instances WHERE id = ?1",
                    params![subject],
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(false);
            Ok(blocked.then_some(("waiting", None)))
        }
        _ => Ok(None),
    }
}

/// Take a row out of delivery: it fell back to the owner push. It keeps no
/// batch id (it was not consumed) and is not offered to a batch again.
fn fall_back(
    conn: &Connection,
    row: &Waiting,
    reason: InboxFallbackReason,
    now: &str,
) -> Result<bool, StoreError> {
    conn.execute(
        "UPDATE inbox_facts SET fallback_at = ?2, fallback_reason = ?3 WHERE id = ?1",
        params![row.id, now, reason.as_str()],
    )?;
    let alert = paging_alert(conn, row)?;
    if let (Some((alert, interaction)), Some(subject)) = (&alert, row.subject.as_deref()) {
        conn.execute(
            "INSERT OR IGNORE INTO inbox_fallback_pushes
                (fact_id, alert, instance_id, interaction_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![row.id, alert, subject, interaction, now],
        )?;
    }
    tracing::info!(
        kind = row.kind.as_str(),
        reason = reason.as_str(),
        paged = alert.is_some(),
        "inbox fact fell back to the owner push"
    );
    Ok(alert.is_some())
}

// ── the Hub's voice ────────────────────────────────────────────────────────

fn text_at<'a>(payload: &'a Value, field: &str) -> Option<&'a str> {
    payload
        .get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
}

/// How a subject instance is named in a line: its id, and the worker name
/// `remuda worker digest` takes when it is a roster worker.
fn subject_label(conn: &Connection, subject: Option<&str>) -> Result<String, StoreError> {
    let Some(subject) = subject else {
        return Ok("(no instance)".to_owned());
    };
    let worker: Option<String> = conn
        .query_row(
            "SELECT name FROM worker_roster
              WHERE json_extract(doc_json, '$.instanceId') = ?1
              ORDER BY updated_at DESC LIMIT 1",
            params![subject],
            |row| row.get(0),
        )
        .optional()?;
    Ok(match worker {
        Some(name) => format!("{} (worker {})", voice::token(subject), voice::token(&name)),
        None => voice::token(subject),
    })
}

/// The subject's last root assistant text, from its journal tail.
fn assistant_tail(conn: &Connection, subject: &str) -> Result<Option<String>, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT payload_json FROM journal WHERE instance_id = ?1 ORDER BY seq DESC LIMIT ?2",
    )?;
    let mut events: Vec<Value> = stmt
        .query_map(params![subject, TAIL_EVENTS], |row| row.get::<_, String>(0))?
        .filter_map(|raw| raw.ok().and_then(|raw| serde_json::from_str(&raw).ok()))
        .collect();
    events.reverse();
    Ok(crate::digest::last_assistant_text(&events)
        .map(|text| crate::digest::redact_secrets(&text))
        .filter(|text| !text.trim().is_empty()))
}

/// The question an interaction row asks (secrets redacted).
fn interaction_question(
    conn: &Connection,
    interaction_id: &str,
) -> Result<Option<String>, StoreError> {
    let record = conn
        .query_row(
            "SELECT instance_id, host_id, kind, state, blocking, payload_json, created_at,
                    updated_at
               FROM interactions WHERE id = ?1",
            params![interaction_id],
            |row| {
                Ok(crate::store::InteractionRecord {
                    interaction_id: interaction_id.to_owned(),
                    instance_id: row.get(0)?,
                    host_id: row.get(1)?,
                    kind: row.get(2)?,
                    state: row.get(3)?,
                    blocking: row.get(4)?,
                    payload: serde_json::from_str(&row.get::<_, String>(5)?).unwrap_or(Value::Null),
                    created_at: row.get(6)?,
                    updated_at: row.get(7)?,
                })
            },
        )
        .optional()?;
    let Some(record) = record else {
        return Ok(None);
    };
    let (_, question) = crate::digest::interaction_question(&record);
    Ok(Some(question).filter(|question| !question.trim().is_empty()))
}

/// `; <what>, untrusted: "<quoted>"` for a worker- or tool-produced text.
fn quoted(what: &str, text: Option<&str>, tail: bool) -> String {
    match text {
        Some(text) => {
            let bounded = if tail {
                voice::clip_tail(text)
            } else {
                voice::clip_head(text)
            };
            format!("; {what}, untrusted: {}", untrusted(&bounded))
        }
        None => String::new(),
    }
}

/// One fact in the Hub's voice: what happened to whom, in the third person,
/// with any worker- or tool-produced text quoted as untrusted data. Never
/// the owner's words: a row's payload carries none, and the only texts read
/// here are the subject's own (its question, its last assistant message,
/// its error).
fn line(conn: &Connection, row: &Waiting) -> Result<Line, StoreError> {
    let who = subject_label(conn, row.subject.as_deref())?;
    let p = &row.payload;
    let at = &row.entered_at;
    let body = match row.kind {
        InboxFactKind::Started => format!(
            "{who} started ({}) at {at}.",
            voice::token(text_at(p, "lifecycle").unwrap_or("running"))
        ),
        InboxFactKind::Exited => format!(
            "{who} ended ({}) at {at}{}.",
            voice::token(text_at(p, "lifecycle").unwrap_or("exited")),
            quoted("last error", text_at(p, "lastError"), false)
        ),
        InboxFactKind::Waiting => format!(
            "{who} is blocked since {at} with no question the Hub holds (its screen may be \
             waiting for input)."
        ),
        InboxFactKind::TurnDone | InboxFactKind::TurnFailed => {
            let failed = row.kind == InboxFactKind::TurnFailed;
            let what = if failed { "failed" } else { "finished" };
            let run = if p.get("unattributed").and_then(Value::as_bool) == Some(true) {
                "a turn the Hub cannot place".to_owned()
            } else {
                format!("turn {}", row.run)
            };
            let late = match p.get("currentRun").and_then(Value::as_i64) {
                Some(current) if p.get("late").and_then(Value::as_bool) == Some(true) => {
                    format!(" (late: turn {current} is the current one)")
                }
                _ => String::new(),
            };
            let detail = if failed {
                quoted("error", text_at(p, "error"), false)
            } else {
                match row.subject.as_deref() {
                    Some(subject) => {
                        quoted("last text", assistant_tail(conn, subject)?.as_deref(), true)
                    }
                    None => String::new(),
                }
            };
            format!("{who} {what} {run} at {at}{late}{detail}.")
        }
        InboxFactKind::Interaction => {
            let id = text_at(p, "interactionId").unwrap_or("?");
            let kind = text_at(p, "interactionKind").unwrap_or("interaction");
            let route = if p.get("routedToParent").and_then(Value::as_bool) == Some(true) {
                "routed to this lineage: answer it with remuda instance respond"
            } else {
                "the owner answers it"
            };
            let question = interaction_question(conn, id)?;
            format!(
                "{who} asks a {} ({}, {route}) since {at}{}.",
                voice::token(kind),
                voice::token(id),
                quoted("question", question.as_deref(), false)
            )
        }
        InboxFactKind::InteractionSuperseded => format!(
            "{who}'s {} {} was superseded at {at} by {}.",
            voice::token(text_at(p, "interactionKind").unwrap_or("interaction")),
            voice::token(text_at(p, "interactionId").unwrap_or("?")),
            voice::token(text_at(p, "supersededBy").unwrap_or("?")),
        ),
        InboxFactKind::Gate => {
            let branch = match text_at(p, "branch") {
                Some(branch) => format!(
                    ", branch untrusted: {}",
                    untrusted(&voice::clip_head(branch))
                ),
                None => String::new(),
            };
            format!(
                "gate job {} ended {} at {at}{branch}.",
                voice::token(text_at(p, "jobId").unwrap_or("?")),
                voice::token(text_at(p, "state").unwrap_or("?")),
            )
        }
        InboxFactKind::SupplyReset => format!(
            "{who} was parked on the {} window of profile {}; it reset at {at}.",
            voice::token(text_at(p, "family").unwrap_or("?")),
            voice::token(text_at(p, "profileId").unwrap_or("?")),
        ),
        InboxFactKind::HostOnline => format!(
            "host {} is back online at {at}.",
            voice::token(text_at(p, "hostId").unwrap_or("?"))
        ),
        InboxFactKind::HostOffline => format!(
            "host {} has been offline since {at}.",
            voice::token(text_at(p, "hostId").unwrap_or("?"))
        ),
        InboxFactKind::Stalled => match text_at(p, "toolCallId") {
            Some(call) => format!(
                "{who} has had tool call {} in flight since {at}.",
                voice::token(call)
            ),
            None => format!(
                "{who} has been quiet since {at} (window {} min).",
                p.get("thresholdMins").and_then(Value::as_i64).unwrap_or(0)
            ),
        },
        InboxFactKind::BudgetExpired => format!(
            "task {}'s wall budget of {} min expired at {at}; {who} keeps running.",
            voice::token(text_at(p, "taskId").unwrap_or("?")),
            p.get("maxWallMins").and_then(Value::as_i64).unwrap_or(0)
        ),
        InboxFactKind::WakeDue | InboxFactKind::ContextThreshold => {
            format!("{who}: {} at {at}.", row.kind.as_str())
        }
    };
    let occurrence = match (
        p.pointer("/occurrence/count").and_then(Value::as_i64),
        p.pointer("/occurrence/capped").and_then(Value::as_i64),
    ) {
        (Some(count), Some(capped)) if count > 1 => format!(
            " Seen {count} times in {}h{}.",
            p.pointer("/occurrence/windowHours")
                .and_then(Value::as_i64)
                .unwrap_or(remuda_protocol::FACT_OCCURRENCE_WINDOW_HOURS),
            if capped > 0 {
                format!(", {capped} of them not delivered (cap)")
            } else {
                String::new()
            }
        ),
        _ => String::new(),
    };
    Ok(Line {
        kind: row.kind,
        text: format!("- [{}] {body}{occurrence}", row.kind.as_str()),
    })
}

/// The lineage's live children by activity, for the footer.
fn snapshot(conn: &Connection, lineage_id: &str) -> Result<Snapshot, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT c.activity FROM instances c
          WHERE c.ended_at IS NULL AND c.fenced_at IS NULL
            AND c.lifecycle NOT IN ('exited', 'failed', 'closed')
            AND COALESCE(c.lineage_id, c.id) <> ?1
            AND json_extract(c.spec_json, '$.parentInstanceId') IN
                (SELECT p.id FROM instances p WHERE COALESCE(p.lineage_id, p.id) = ?1)",
    )?;
    let mut snapshot = Snapshot::default();
    for activity in stmt.query_map(params![lineage_id], |row| row.get::<_, String>(0))? {
        match activity?.as_str() {
            "working" | "draining" => snapshot.working += 1,
            "blocked" => snapshot.waiting += 1,
            "idle" => snapshot.idle += 1,
            _ => snapshot.other += 1,
        }
    }
    Ok(snapshot)
}

// ── formation ──────────────────────────────────────────────────────────────

/// What one evaluation of a due lineage did.
#[derive(Debug, Default, Clone, Copy)]
struct Formed {
    /// A batch was formed: the driver has a forward to make.
    batch: bool,
    /// Rows fell back with an owner push to send.
    pushes: bool,
}

/// Whether `lineage_id` has a formed batch still queued and unforwarded.
fn unsent_batch(conn: &Connection, lineage_id: &str) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        concat!("SELECT ", unsent_batch!("?1")),
        params![lineage_id],
        |row| row.get(0),
    )?)
}

/// The `instance.send` payload of a batch: Hub-written input, origin `hub`
/// (D-057 §6.4), like the restart notice.
fn send_payload(chapter: &str, text: &str) -> Value {
    json!({
        "instanceId": chapter,
        "origin": "hub",
        "input": { "type": "prompt", "text": text, "origin": "hub" },
    })
}

fn form_one(conn: &Connection, lineage_id: &str) -> Result<Formed, StoreError> {
    let chapter = match gate(conn, lineage_id)? {
        Ok(chapter) => chapter,
        Err(Hold::Gone) => {
            // Neither held forever nor consumed (§10.1 item 4).
            let now = now_rfc3339();
            let mut formed = Formed::default();
            for row in waiting(conn, lineage_id)? {
                formed.pushes |= fall_back(conn, &row, InboxFallbackReason::NoSubscriber, &now)?;
            }
            return Ok(formed);
        }
        Err(hold) => {
            tracing::debug!(hold = hold.label(), "inbox batch held");
            // A batch formed earlier still waits in the queue (owner input
            // arrived before its forward, or a forward was refused): the
            // state change that queued this evaluation may let it go now.
            return Ok(Formed {
                batch: hold == Hold::InFlight && unsent_batch(conn, lineage_id)?,
                pushes: false,
            });
        }
    };
    let rows = waiting(conn, lineage_id)?;
    if rows.is_empty() {
        return Ok(Formed::default());
    }
    let muted = crate::inbox::load_settings(conn, lineage_id)?.muted_kinds;
    let now = now_rfc3339();
    let mut formed = Formed::default();
    let mut carried = Vec::with_capacity(rows.len());
    for row in rows {
        match recheck(conn, lineage_id, &muted, &row)? {
            None => carried.push(row),
            Some(reason) => formed.pushes |= fall_back(conn, &row, reason, &now)?,
        }
    }
    if carried.is_empty() {
        return Ok(formed);
    }
    let batch_id = crate::config::new_id("cmd").map_err(|e| StoreError::Id(e.to_string()))?;
    let lines = carried
        .iter()
        .map(|row| line(conn, row))
        .collect::<Result<Vec<_>, _>>()?;
    let composed = voice::compose(
        &voice::header(&batch_id, lineage_id, chapter.generation, carried.len()),
        &lines,
        &voice::footer(lineage_id, &now, snapshot(conn, lineage_id)?),
        lineage_id,
    );
    let fact_ids: Vec<i64> = carried.iter().map(|row| row.id).collect();
    // §10, Ids: the id is chosen and the row persisted now, with the
    // Hub-internal initiator for the live chapter's generation and no device
    // (§7.1), so F's step 4 covers it and no Hub crash can send it twice.
    conn.execute(
        "INSERT INTO commands
            (id, instance_id, host_id, operation, state, resolution, forwarded,
             payload_json, idempotency_key, created_at, updated_at,
             initiator_instance_id, initiator_lineage_id, initiator_generation)
         VALUES (?1, ?2, ?3, 'instance.send', 'queued', 'clear', 0, ?4, NULL, ?5, ?5,
                 ?2, ?6, ?7)",
        params![
            batch_id,
            chapter.instance_id,
            chapter.host_id,
            send_payload(&chapter.instance_id, &composed.text).to_string(),
            now,
            lineage_id,
            chapter.generation,
        ],
    )?;
    // §10.1 item 6: the visible row commits with the command row, before any
    // forward intent exists.
    conn.execute(
        "INSERT INTO inbox_batches
            (batch_id, lineage_id, instance_id, generation, host_id, opening_tag,
             template_version, fact_ids_json, listed, text, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            batch_id,
            lineage_id,
            chapter.instance_id,
            chapter.generation,
            chapter.host_id,
            voice::OPENING_TAG,
            voice::TEMPLATE_VERSION,
            serde_json::to_string(&fact_ids)?,
            i64::try_from(composed.listed).unwrap_or(i64::MAX),
            composed.text,
            now,
        ],
    )?;
    {
        let mut stamp = conn.prepare_cached(
            "UPDATE inbox_facts SET batch_id = ?2 WHERE id = ?1 AND batch_id IS NULL",
        )?;
        for id in &fact_ids {
            stamp.execute(params![id, batch_id])?;
        }
    }
    let mut kinds: Vec<(&str, usize)> = Vec::new();
    for row in &carried {
        match kinds
            .iter_mut()
            .find(|(kind, _)| *kind == row.kind.as_str())
        {
            Some((_, count)) => *count += 1,
            None => kinds.push((row.kind.as_str(), 1)),
        }
    }
    let kinds = kinds
        .iter()
        .map(|(kind, count)| format!("{kind}={count}"))
        .collect::<Vec<_>>()
        .join(",");
    tracing::info!(
        facts = carried.len(),
        listed = composed.listed,
        bytes = composed.text.len(),
        template = voice::TEMPLATE_VERSION,
        kinds = %kinds,
        "inbox batch formed"
    );
    formed.batch = true;
    Ok(formed)
}

/// Evaluate every due lineage on the writer connection, after a writer job
/// (and once at open). Returns whether the driver has work: a batch to
/// forward or an owner push to send. A lineage whose gate is closed is
/// dropped from the due set: only its next state change re-queues it, so a
/// held lineage costs nothing until then.
pub(crate) fn form_due(conn: &mut Connection) -> bool {
    let due = conn
        .prepare_cached("SELECT EXISTS (SELECT 1 FROM inbox_deliver_due)")
        .and_then(|mut stmt| stmt.query_row([], |row| row.get::<_, bool>(0)));
    match due {
        Ok(true) => {}
        Ok(false) => return false,
        Err(err) => {
            tracing::error!(
                code = error_code(&err.into()),
                "inbox delivery probe failed"
            );
            return false;
        }
    }
    let run = |conn: &mut Connection| -> Result<bool, StoreError> {
        let tx = crate::store::immediate_tx(conn)?;
        let lineages: Vec<String> = {
            let mut stmt = tx.prepare_cached("SELECT lineage_id FROM inbox_deliver_due")?;
            stmt.query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()?
        };
        tx.execute("DELETE FROM inbox_deliver_due", [])?;
        let mut work = false;
        for lineage in lineages {
            // A lineage whose formation fails keeps its rows waiting; it is
            // evaluated again on its next state change.
            if let Some(formed) =
                crate::inbox::guard("inbox-deliver", &tx, || form_one(&tx, &lineage))
            {
                work |= formed.batch || formed.pushes;
            }
        }
        tx.commit()?;
        Ok(work)
    };
    match run(conn) {
        Ok(work) => work,
        Err(err) => {
            tracing::error!(code = error_code(&err), "inbox delivery failed");
            false
        }
    }
}

/// A log-safe code for an error: never its message, which can carry text.
fn error_code(err: &StoreError) -> String {
    match err {
        StoreError::Sqlite(rusqlite::Error::SqliteFailure(failure, _)) => {
            format!("sqlite-{}", failure.extended_code)
        }
        StoreError::Sqlite(_) => "sqlite".into(),
        StoreError::Json(_) => "json".into(),
        StoreError::Closed => "closed".into(),
        _ => "store".into(),
    }
}

// ── reads ──────────────────────────────────────────────────────────────────

/// The batch rows named by `ids`, oldest first.
pub(crate) fn batches(conn: &Connection, ids: &[String]) -> Result<Vec<InboxBatch>, StoreError> {
    let mut out = Vec::with_capacity(ids.len());
    let mut stmt = conn.prepare_cached(
        "SELECT batch_id, lineage_id, instance_id, generation, opening_tag, template_version,
                fact_ids_json, listed, text, created_at, outcome, released_at
           FROM inbox_batches WHERE batch_id = ?1",
    )?;
    for id in ids {
        let row = stmt
            .query_row(params![id], |row| {
                Ok(InboxBatch {
                    batch_id: row.get(0)?,
                    lineage_id: row.get(1)?,
                    instance_id: row.get(2)?,
                    generation: row.get(3)?,
                    opening_tag: row.get(4)?,
                    template_version: row.get(5)?,
                    fact_ids: serde_json::from_str(&row.get::<_, String>(6)?).unwrap_or_default(),
                    listed: row.get(7)?,
                    text: row.get(8)?,
                    created_at: row.get(9)?,
                    outcome: row.get(10)?,
                    released_at: row.get(11)?,
                })
            })
            .optional()?;
        out.extend(row);
    }
    out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(out)
}

// ── the driver: forwards and owner pushes ──────────────────────────────────

/// In-process state of the delivery driver.
#[derive(Clone, Default)]
pub(crate) struct DeliverState {
    /// Drives are serialized, so two never forward or push the same row at
    /// once (the forward's own leader slot and the push's sent mark are the
    /// durable guards).
    drive: std::sync::Arc<tokio::sync::Mutex<()>>,
    /// Batches released back to the queue after a forward that provably did
    /// not reach the chapter (NODE_BUSY, the link gone, a Node refusal before
    /// it recorded anything), by batch id: the same id is retried once its
    /// backoff has passed. In memory: after a Hub restart every unsent batch
    /// is tried again at once.
    retry: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Retry>>>,
    /// Test seam: hold every forward (a batch forms and stays queued).
    #[cfg(any(test, feature = "test-faults"))]
    pub(crate) hold_forward: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// One batch's same-id retry schedule.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Retry {
    attempts: u32,
    next: tokio::time::Instant,
}

/// First retry delay after a forward that did not reach the chapter.
const RETRY_FIRST: std::time::Duration = std::time::Duration::from_secs(1);
/// Longest retry delay: the backoff doubles up to this and stays there.
const RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(60);

/// The delay before attempt `attempts + 1`: 1 s, 2 s, 4 s, … capped at
/// [`RETRY_MAX`].
pub(crate) fn retry_delay(attempts: u32) -> std::time::Duration {
    let factor = 1_u32
        .checked_shl(attempts.saturating_sub(1))
        .unwrap_or(u32::MAX);
    RETRY_FIRST.saturating_mul(factor).min(RETRY_MAX)
}

/// Whether a driver pass honours the retry backoff: the periodic tick and
/// the writer's signal do; boot and an explicit test drive do not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pace {
    Backoff,
    Now,
}

impl DeliverState {
    fn retry_due(&self, batch_id: &str, now: tokio::time::Instant) -> bool {
        self.retry
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(batch_id)
            .is_none_or(|retry| retry.next <= now)
    }

    /// Schedule the next same-id attempt; returns the attempt count.
    fn note_retry(&self, batch_id: &str) -> (u32, std::time::Duration) {
        let mut retry = self
            .retry
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let attempts = retry.get(batch_id).map_or(0, |r| r.attempts) + 1;
        let delay = retry_delay(attempts);
        retry.insert(
            batch_id.to_owned(),
            Retry {
                attempts,
                next: tokio::time::Instant::now() + delay,
            },
        );
        (attempts, delay)
    }

    fn clear_retry(&self, batch_id: &str) {
        self.retry
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(batch_id);
    }

    /// Whether some released batch's backoff has passed.
    fn retry_pending(&self, now: tokio::time::Instant) -> bool {
        self.retry
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .values()
            .any(|retry| retry.next <= now)
    }
}

#[cfg(any(test, feature = "test-faults"))]
fn forward_held(state: &AppState) -> bool {
    state
        .deliver
        .hold_forward
        .load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(not(any(test, feature = "test-faults")))]
fn forward_held(_state: &AppState) -> bool {
    false
}

/// A formed batch whose send has not been forwarded yet.
struct Unsent {
    batch_id: String,
    host_id: String,
}

/// An owner push a fallen-back row still owes.
struct Owed {
    fact_id: i64,
    alert: String,
    instance_id: String,
    interaction_id: Option<String>,
}

/// Forward every formed batch whose send is still queued and unforwarded,
/// then send the owner pushes fallen-back rows owe. Run after the writer
/// formed something, after a host's hello, and at boot.
///
/// Each batch goes through `forward_if_online` with the id it was formed
/// with: the forward intent is its replay identity (a row already forwarded
/// is never written again, and an unknown outcome is left to the Node's
/// journal to settle on that id); a host with unacknowledged fences holds it
/// until they are acknowledged; the send gate withholds it if its chapter
/// was fenced meanwhile.
pub(crate) async fn drive(state: &AppState, pace: Pace) {
    let _drive = state.deliver.drive.lock().await;
    let unsent = state
        .store
        .read("inbox_batches_unsent", |conn| {
            let mut stmt = conn.prepare(
                "SELECT b.batch_id, b.host_id FROM inbox_batches b
                   JOIN commands c ON c.id = b.batch_id
                  WHERE b.outcome IS NULL AND c.state = 'queued' AND c.forwarded = 0
                  ORDER BY b.created_at",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(Unsent {
                        batch_id: row.get(0)?,
                        host_id: row.get(1)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await;
    match unsent {
        Ok(unsent) if !forward_held(state) => {
            let now = tokio::time::Instant::now();
            // A batch that left the queue (forwarded, cancelled by a fence)
            // has no retry left to make.
            state
                .deliver
                .retry
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .retain(|id, _| unsent.iter().any(|batch| &batch.batch_id == id));
            for batch in unsent {
                if pace == Pace::Backoff && !state.deliver.retry_due(&batch.batch_id, now) {
                    continue;
                }
                if state.nodes.kind_of(&batch.host_id).await.is_none() {
                    // Offline: the host's next hello drives it.
                    continue;
                }
                if waits_behind(state, &batch.batch_id).await {
                    // Input that arrived after the batch formed goes first;
                    // its settling re-drives this batch.
                    state.deliver.clear_retry(&batch.batch_id);
                    tracing::debug!("inbox batch waits behind input that goes first");
                    continue;
                }
                let command = match state.store.get_command(batch.batch_id.clone()).await {
                    Ok(Some(command)) => command,
                    Ok(None) => continue,
                    Err(err) => {
                        tracing::warn!(code = error_code(&err), "inbox batch read failed");
                        continue;
                    }
                };
                match crate::http::forward_if_online(state, command, true).await {
                    Ok(row)
                        if row.state == "queued"
                            && !row.forwarded
                            && waits_behind(state, &batch.batch_id).await =>
                    {
                        // Held in the writer job that would have marked it:
                        // input went first in between.
                        state.deliver.clear_retry(&batch.batch_id);
                        tracing::debug!("inbox batch waits behind input that goes first");
                    }
                    Ok(row) if row.state == "queued" && !row.forwarded => {
                        // Back in the queue: the forward proved it never
                        // reached the chapter (NODE_BUSY, the link gone, a
                        // refusal before the Node recorded it) or a fence
                        // holds its host. The same id is tried again after
                        // the backoff.
                        let (attempts, delay) = state.deliver.note_retry(&batch.batch_id);
                        tracing::info!(
                            attempts,
                            retry_in_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                            "inbox batch not delivered; retried under the same id"
                        );
                    }
                    Ok(row) => {
                        state.deliver.clear_retry(&batch.batch_id);
                        tracing::info!(
                            state = row.state.as_str(),
                            forwarded = row.forwarded,
                            "inbox batch forward"
                        );
                    }
                    Err(err) => {
                        let (status, body) = err.status_code_and_body();
                        let code = body
                            .get("code")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        let (attempts, _) = state.deliver.note_retry(&batch.batch_id);
                        tracing::warn!(status, code = %code, attempts, "inbox batch forward failed");
                    }
                }
            }
        }
        Ok(_) => {}
        Err(err) => tracing::warn!(code = error_code(&err), "inbox batches read failed"),
    }
    push_owed(state).await;
}

/// [`forward_waits`] on the reader pool; a read failure does not hold.
async fn waits_behind(state: &AppState, batch_id: &str) -> bool {
    let id = batch_id.to_owned();
    state
        .store
        .read("inbox_batch_waits", move |conn| forward_waits(conn, &id))
        .await
        .unwrap_or(false)
}

/// Scheduler ticks between two delivery retry checks (the 1 s schedule
/// tick).
pub(crate) const RETRY_TICK_EVERY: u64 = 5;

/// The periodic retry check: a driver pass, honouring the backoff, when a
/// released batch's backoff has passed. One in-memory look; nothing polls a
/// Node.
pub(crate) async fn retry_tick(state: &AppState) {
    if state.deliver.retry_pending(tokio::time::Instant::now()) {
        drive(state, Pace::Backoff).await;
    }
}

/// Send each owner push a fallen-back row owes through `alerts.rs`, and
/// mark it sent only after the fanout delivered it (or found nothing to
/// deliver to). A failed fanout stays owed for the next drive; the alert's
/// own tag makes a resend replace the notification instead of adding one.
async fn push_owed(state: &AppState) {
    let owed = state
        .store
        .read("inbox_fallback_owed", |conn| {
            let mut stmt = conn.prepare(
                "SELECT fact_id, alert, instance_id, interaction_id
                   FROM inbox_fallback_pushes WHERE recipients IS NULL ORDER BY fact_id",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(Owed {
                        fact_id: row.get(0)?,
                        alert: row.get(1)?,
                        instance_id: row.get(2)?,
                        interaction_id: row.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await;
    let owed = match owed {
        Ok(owed) => owed,
        Err(err) => {
            tracing::warn!(code = error_code(&err), "inbox fallback pushes read failed");
            return;
        }
    };
    for push in owed {
        let alert = match (push.alert.as_str(), push.interaction_id) {
            ("interaction", Some(id)) => crate::alerts::Paged::Interaction(id),
            ("exited", _) => crate::alerts::Paged::Exited,
            ("waiting", _) => crate::alerts::Paged::Waiting,
            _ => crate::alerts::Paged::Nothing,
        };
        let fanout = crate::alerts::page_owner(state, &push.instance_id, alert).await;
        let Some(recipients) = fanout.recipients() else {
            tracing::warn!(
                alert = push.alert.as_str(),
                "inbox fallback push failed; it is retried"
            );
            continue;
        };
        tracing::info!(
            alert = push.alert.as_str(),
            recipients,
            "inbox fallback push"
        );
        let fact_id = push.fact_id;
        if let Err(err) = state
            .store
            .run_named("inbox_fallback_sent", move |conn| {
                conn.execute(
                    "UPDATE inbox_fallback_pushes SET sent_at = ?2, recipients = ?3
                      WHERE fact_id = ?1 AND recipients IS NULL",
                    params![
                        fact_id,
                        now_rfc3339(),
                        i64::try_from(recipients).unwrap_or(i64::MAX)
                    ],
                )?;
                Ok(())
            })
            .await
        {
            tracing::warn!(code = error_code(&err), "inbox fallback push mark failed");
        }
    }
}

/// Whether the Hub delivers inbox facts to `instance_id`'s lineage (the skill's
/// `inbox` flag): the lineage receives facts (§10, Defaults) and its host
/// takes hub-origin input.
pub(crate) fn delivers_to(conn: &Connection, instance_id: &str) -> Result<bool, StoreError> {
    let Some(lineage) = crate::store::lineage_id_of(conn, instance_id)? else {
        return Ok(false);
    };
    let host: Option<String> = conn
        .query_row(
            "SELECT host_id FROM instances WHERE id = ?1",
            params![instance_id],
            |row| row.get(0),
        )
        .optional()?;
    let capable = match host {
        Some(host) => crate::fence::host_capable(conn, &host)?,
        None => false,
    };
    Ok(capable && crate::inbox::receives_any(conn, &lineage)?)
}
