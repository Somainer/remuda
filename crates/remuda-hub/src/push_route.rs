//! D-057 §8 (ma-push): push routing for agent-parented alerts.
//!
//! **Routing (§8.1).** An interaction alert about instance I is not pushed
//! when, at fire time, I has an Agent parent whose lineage is P, the item was
//! actually routed to P (a non-approval kind the D-051 predicate admits for
//! P's current chapter) and P is *effectively running*: stored state
//! `running`, its current chapter at the live generation, unfenced and not
//! terminal, on a host whose link is live. Every suppression writes one
//! durable row in `push_suppressions` (interaction, lineage, chapter, rule):
//! it is also the audit row (§8.6).
//!
//! **The lift (§8.3).** [`transition`] is the one function that computes
//! when "effectively running" turns from true to false. Every cause calls it
//! inside its own writer job ([`Transition`] names them); for a lineage that
//! is no longer effectively running it marks each still-pending suppressed
//! interaction lifted and inserts one `push_outbox` row per suppression
//! (keyed by the interaction), so a second transition adds nothing. After
//! commit the outbox is sent with today's body and the interaction's tag,
//! then marked delivered ([`drive_outbox`]); an undelivered row is sent
//! again after a Hub restart, and the tag makes a resend replace the first
//! notification. A lifted interaction is never suppressed again.
//!
//! **Reply-ready (§8.5).** [`fold_reply`] runs in the journal append
//! transaction: a Human-origin input to an `address-owner` chapter arms its
//! tracker, and the settled end of that turn on the live chapter queues one
//! "Main replied: …" outbox row.
//!
//! **Push log (§8.6).** Every fanout records one `push_log` row: category,
//! tag, counts and the devices it reached or skipped, never a body.

use crate::AppState;
use crate::store::{StoreError, immediate_tx, lineage_id_of, parent_lineage_of};
use remuda_push::PushTag;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::cell::Cell;
use std::sync::Arc;

/// The §8.1 rule a suppression row records.
pub(crate) const RULE_ROUTED_TO_RUNNING_PARENT: &str = "routed-to-running-parent";

/// Interaction kinds that can be routed to an Agent parent (§8.1 condition
/// 2). Approval-kind items never route (D-051(a)); a kind this list does not
/// name (a legacy `permission` row, a future kind) pages as today.
const ROUTABLE_KINDS: &[&str] = &["question", "plan-review", "elicitation"];

/// Push log rows kept; older rows are pruned by the insert.
const PUSH_LOG_KEEP: i64 = 5000;

/// Characters of the final message's first line in "Main replied: …".
const REPLY_LINE_CHARS: usize = 80;

/// Journal rows read back to find a turn's final message.
const REPLY_SCAN_ROWS: i64 = 400;

/// Idempotent DDL; no `user_version` rung.
pub(crate) fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS push_suppressions (
            interaction_id TEXT PRIMARY KEY,
            instance_id TEXT NOT NULL,
            interaction_kind TEXT NOT NULL,
            lineage_id TEXT NOT NULL,
            chapter_instance_id TEXT NOT NULL,
            chapter_generation INTEGER NOT NULL,
            rule TEXT NOT NULL,
            suppressed_at TEXT NOT NULL,
            lifted_at TEXT,
            lift_cause TEXT
         );
         CREATE INDEX IF NOT EXISTS push_suppressions_active
            ON push_suppressions(lineage_id) WHERE lifted_at IS NULL;
         CREATE TABLE IF NOT EXISTS push_outbox (
            outbox_key TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            instance_id TEXT NOT NULL,
            interaction_id TEXT,
            tag_kind TEXT NOT NULL,
            tag_id TEXT NOT NULL,
            title TEXT NOT NULL,
            body TEXT NOT NULL,
            url TEXT NOT NULL,
            created_at TEXT NOT NULL,
            sent_at TEXT,
            recipients INTEGER
         );
         CREATE INDEX IF NOT EXISTS push_outbox_unsent
            ON push_outbox(created_at) WHERE recipients IS NULL;
         CREATE TABLE IF NOT EXISTS push_log (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            at TEXT NOT NULL,
            category TEXT NOT NULL,
            tag TEXT NOT NULL,
            instance_id TEXT NOT NULL,
            outcome TEXT NOT NULL,
            sent INTEGER NOT NULL,
            gone INTEGER NOT NULL,
            failed INTEGER NOT NULL,
            suppressed INTEGER NOT NULL,
            delivered_devices_json TEXT NOT NULL,
            suppressed_devices_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS push_reply_trackers (
            instance_id TEXT NOT NULL,
            command_id TEXT NOT NULL,
            armed_seq INTEGER NOT NULL,
            armed_at TEXT NOT NULL,
            PRIMARY KEY (instance_id, command_id)
         );",
    )
}

thread_local! {
    /// Set on the writer thread when a job inserted outbox rows; the writer
    /// loop takes it after the job and wakes the outbox drive.
    static OUTBOX_DIRTY: Cell<bool> = const { Cell::new(false) };
}

fn note_outbox() {
    OUTBOX_DIRTY.with(|dirty| dirty.set(true));
}

/// Whether the job that just ran on this thread queued outbox rows.
pub(crate) fn take_outbox_dirty() -> bool {
    OUTBOX_DIRTY.with(|dirty| dirty.replace(false))
}

/// Run a push hook inside a savepoint: push bookkeeping never fails the
/// write it rides on (a fence, a link loss, a journal append). A failure
/// rolls the hook back and is logged by category.
pub(crate) fn guard<T>(
    hook: &'static str,
    conn: &Connection,
    run: impl FnOnce() -> Result<T, StoreError>,
) -> Option<T> {
    if conn.execute_batch("SAVEPOINT push_hook").is_err() {
        tracing::error!(
            hook,
            "push hook savepoint failed; its write commits without it"
        );
        return None;
    }
    match run() {
        Ok(value) => {
            if conn.execute_batch("RELEASE push_hook").is_err() {
                tracing::error!(hook, "push hook savepoint release failed");
            }
            Some(value)
        }
        Err(_) => {
            let _ = conn.execute_batch("ROLLBACK TO push_hook; RELEASE push_hook");
            tracing::error!(hook, "push hook failed; its write commits without it");
            None
        }
    }
}

// ─── the effectively-running rule ───────────────────────────────────────────

/// §8.1 condition 3: `lineage_id`'s current chapter and generation when the
/// lineage is effectively running — stored state `running`, the chapter at
/// the live generation, unfenced and not terminal-shaped, on a host whose
/// link is live. A chapter whose row reads terminal without process-end
/// evidence (a host-contact-lost sweep) is not known to run, so it does not
/// count: the rule is a positive claim, and its failure pages.
pub(crate) fn effectively_running(
    conn: &Connection,
    lineage_id: &str,
) -> Result<Option<(String, i64)>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT l.current_instance_id, l.generation
               FROM lineages l
               JOIN instances i ON i.id = l.current_instance_id
               JOIN hosts h ON h.id = i.host_id
              WHERE l.lineage_id = ?1 AND l.state = 'running'
                AND i.generation = l.generation AND i.fenced_at IS NULL
                AND i.lifecycle NOT IN ('exited', 'failed', 'closed')
                AND h.state = 'online'",
            params![lineage_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
}

/// §8.3 boot step 2: running *by state*, without the host link (every host
/// reads offline at boot as bookkeeping, not as an observation).
fn running_by_state(conn: &Connection, lineage_id: &str) -> Result<bool, StoreError> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM lineages l
               JOIN instances i ON i.id = l.current_instance_id
              WHERE l.lineage_id = ?1 AND l.state = 'running'
                AND i.generation = l.generation AND i.fenced_at IS NULL
                AND i.lifecycle NOT IN ('exited', 'failed', 'closed')",
            params![lineage_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

// ─── fire-time routing ──────────────────────────────────────────────────────

/// What an interaction alert does at fire time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    /// Page the owner as today.
    Page,
    /// Routed to an effectively running Agent parent: not pushed. The row
    /// was written (or already existed, unlifted).
    Suppressed,
}

/// §8.1: the parent lineage, its chapter and generation when an alert about
/// `kind` raised by `instance_id` is routed to an effectively running Agent
/// parent; `None` pages.
fn routed_parent(
    conn: &Connection,
    instance_id: &str,
    kind: &str,
) -> Result<Option<(String, String, i64)>, StoreError> {
    if !ROUTABLE_KINDS.contains(&kind) {
        return Ok(None);
    }
    // 1. An Agent parent: the create-time parent edge names an instance
    //    (a Human-parented instance has none), outside I's own lineage.
    let Some(parent) = parent_lineage_of(conn, instance_id)? else {
        return Ok(None);
    };
    if lineage_id_of(conn, instance_id)?.as_deref() == Some(parent.as_str()) {
        return Ok(None);
    }
    // 2. Actually routed: the D-051 predicate for P's current chapter.
    if !crate::interactions::routed_to_parent_conn(conn, instance_id, kind)? {
        return Ok(None);
    }
    // 3. P effectively running.
    Ok(effectively_running(conn, &parent)?
        .map(|(chapter, generation)| (parent, chapter, generation)))
}

/// Decide an interaction alert at fire time, in one writer transaction with
/// the suppression row it may write, so it is totally ordered against every
/// [`transition`].
pub(crate) fn route_interaction(
    conn: &mut Connection,
    interaction_id: &str,
    instance_id: &str,
) -> Result<Route, StoreError> {
    let tx = immediate_tx(conn)?;
    let existing: Option<Option<String>> = tx
        .query_row(
            "SELECT lifted_at FROM push_suppressions WHERE interaction_id = ?1",
            params![interaction_id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(lifted) = existing {
        // §8.3: a lifted interaction is never suppressed again.
        return Ok(if lifted.is_some() {
            Route::Page
        } else {
            Route::Suppressed
        });
    }
    let row: Option<(String, String)> = tx
        .query_row(
            "SELECT kind, state FROM interactions WHERE id = ?1 AND instance_id = ?2",
            params![interaction_id, instance_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((kind, state)) = row else {
        return Ok(Route::Page);
    };
    if state != "pending" {
        return Ok(Route::Page);
    }
    let Some((lineage, chapter, generation)) = routed_parent(&tx, instance_id, &kind)? else {
        return Ok(Route::Page);
    };
    tx.execute(
        "INSERT INTO push_suppressions
            (interaction_id, instance_id, interaction_kind, lineage_id, chapter_instance_id,
             chapter_generation, rule, suppressed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            interaction_id,
            instance_id,
            kind,
            lineage,
            chapter,
            generation,
            RULE_ROUTED_TO_RUNNING_PARENT,
            crate::config::now_rfc3339()
        ],
    )?;
    tx.commit()?;
    Ok(Route::Suppressed)
}

// ─── the one transition function ────────────────────────────────────────────

/// Every cause of "effectively running" turning false (§8.3). Each names its
/// scope: the lineages it can have changed.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Transition<'a> {
    /// F committed on this lineage: pause, close, restart, cap, resume.
    Fence { lineage_id: &'a str },
    /// The C1 supervisor found this lineage's current chapter ended.
    Supervisor { lineage_id: &'a str },
    /// The journal recorded this chapter's end (between the end and the
    /// supervisor's decision P is not effectively running, §8.2).
    ChapterEnded { instance_id: &'a str },
    /// The host's live link went down: the WS session end in `ws.rs`, the
    /// SSH carrier loss and host removal in `ssh_hosts.rs`.
    LinkLost { host_id: &'a str },
    /// Host-loss detection: the host stayed unreachable past its grace.
    HostLost { host_id: &'a str },
    /// Boot reconciliation, step 2: lineages not running by state.
    Boot,
    /// Boot reconciliation, step 3: the parent host's hello. P stays
    /// effectively running only for the same epoch with its chapter in the
    /// hello's live inventory.
    Hello {
        host_id: &'a str,
        same_epoch: bool,
        live: &'a [String],
    },
}

impl Transition<'_> {
    /// The cause recorded on a lifted row.
    fn cause(&self) -> &'static str {
        match self {
            Self::Fence { .. } => "fence",
            Self::Supervisor { .. } => "supervisor",
            Self::ChapterEnded { .. } => "chapter-ended",
            Self::LinkLost { .. } => "link-lost",
            Self::HostLost { .. } => "host-lost",
            Self::Boot => "boot",
            Self::Hello { .. } => "hello",
        }
    }
}

/// Active suppressions (unlifted, their interaction still pending), by
/// lineage, within `cause`'s scope.
fn scoped_lineages(conn: &Connection, cause: &Transition<'_>) -> Result<Vec<String>, StoreError> {
    const ACTIVE: &str = "SELECT DISTINCT s.lineage_id FROM push_suppressions s
          JOIN interactions i ON i.id = s.interaction_id AND i.state = 'pending'
         WHERE s.lifted_at IS NULL";
    let collect = |sql: String, key: Option<&str>| -> Result<Vec<String>, StoreError> {
        let mut stmt = conn.prepare(&sql)?;
        let rows = match key {
            Some(key) => stmt
                .query_map(params![key], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?,
            None => stmt
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?,
        };
        Ok(rows)
    };
    match cause {
        Transition::Fence { lineage_id } | Transition::Supervisor { lineage_id } => {
            collect(format!("{ACTIVE} AND s.lineage_id = ?1"), Some(lineage_id))
        }
        Transition::ChapterEnded { instance_id } => collect(
            format!(
                "{ACTIVE} AND s.lineage_id IN
                   (SELECT lineage_id FROM lineages WHERE current_instance_id = ?1)"
            ),
            Some(instance_id),
        ),
        Transition::LinkLost { host_id }
        | Transition::HostLost { host_id }
        | Transition::Hello { host_id, .. } => collect(
            format!(
                "{ACTIVE} AND s.lineage_id IN
                   (SELECT l.lineage_id FROM lineages l
                      JOIN instances c ON c.id = l.current_instance_id
                     WHERE c.host_id = ?1)"
            ),
            Some(host_id),
        ),
        Transition::Boot => collect(ACTIVE.to_owned(), None),
    }
}

/// THE §8.3 transition function. For every lineage in `cause`'s scope that
/// has active suppressions and is no longer effectively running, mark each
/// still-pending suppressed interaction lifted and insert its one outbox
/// row. Runs on the caller's transaction, in the cause's own writer job.
/// Returns how many interactions it lifted.
pub(crate) fn transition(conn: &Connection, cause: Transition<'_>) -> Result<usize, StoreError> {
    let mut lifted = 0;
    for lineage in scoped_lineages(conn, &cause)? {
        let running = match &cause {
            Transition::Boot => running_by_state(conn, &lineage)?,
            Transition::Hello {
                same_epoch, live, ..
            } => {
                *same_epoch
                    && effectively_running(conn, &lineage)?
                        .is_some_and(|(chapter, _)| live.contains(&chapter))
            }
            _ => effectively_running(conn, &lineage)?.is_some(),
        };
        if !running {
            lifted += lift_lineage(conn, &lineage, cause.cause())?;
        }
    }
    if lifted > 0 {
        tracing::info!(
            cause = cause.cause(),
            count = lifted,
            "push suppressions lifted"
        );
    }
    Ok(lifted)
}

/// Whether any suppression is still unlifted: with none, no transition has
/// anything to lift. A read error answers yes, so the guarded hook runs and
/// reports it.
pub(crate) fn any_active(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM push_suppressions WHERE lifted_at IS NULL)",
        [],
        |row| row.get::<_, bool>(0),
    )
    .unwrap_or(true)
}

/// [`transition`] inside a savepoint (see [`guard`]); skipped outright while
/// nothing is suppressed.
pub(crate) fn transition_guarded(conn: &Connection, cause: Transition<'_>) {
    if !any_active(conn) {
        return;
    }
    guard("transition", conn, || transition(conn, cause));
}

impl crate::store::Store {
    /// §8.3 boot step 3: the transition a host's hello implies.
    pub(crate) async fn push_hello_transition(
        &self,
        host_id: String,
        same_epoch: bool,
        live: Vec<String>,
    ) -> Result<usize, StoreError> {
        self.run_named("push_hello_transition", move |conn| {
            let tx = immediate_tx(conn)?;
            let lifted = transition(
                &tx,
                Transition::Hello {
                    host_id: &host_id,
                    same_epoch,
                    live: &live,
                },
            )?;
            tx.commit()?;
            Ok(lifted)
        })
        .await
    }

    /// §8.3 boot step 2: lift for every lineage not running by state.
    pub(crate) async fn push_boot_transition(&self) -> Result<usize, StoreError> {
        self.run_named("push_boot_transition", |conn| {
            let tx = immediate_tx(conn)?;
            let lifted = transition(&tx, Transition::Boot)?;
            tx.commit()?;
            Ok(lifted)
        })
        .await
    }

    /// The supervisor's transition for a lineage whose current chapter ended.
    pub(crate) async fn push_supervisor_transition(
        &self,
        lineage_id: String,
    ) -> Result<usize, StoreError> {
        self.run_named("push_supervisor_transition", move |conn| {
            let tx = immediate_tx(conn)?;
            let lifted = transition(
                &tx,
                Transition::Supervisor {
                    lineage_id: &lineage_id,
                },
            )?;
            tx.commit()?;
            Ok(lifted)
        })
        .await
    }
}

fn lift_lineage(conn: &Connection, lineage_id: &str, cause: &str) -> Result<usize, StoreError> {
    let rows: Vec<(String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT s.interaction_id, s.instance_id FROM push_suppressions s
               JOIN interactions i ON i.id = s.interaction_id AND i.state = 'pending'
              WHERE s.lifted_at IS NULL AND s.lineage_id = ?1
              ORDER BY s.suppressed_at, s.interaction_id",
        )?;
        stmt.query_map(params![lineage_id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let now = crate::config::now_rfc3339();
    let mut lifted = 0;
    for (interaction_id, instance_id) in rows {
        let changed = conn.execute(
            "UPDATE push_suppressions SET lifted_at = ?2, lift_cause = ?3
              WHERE interaction_id = ?1 AND lifted_at IS NULL",
            params![interaction_id, now, cause],
        )?;
        if changed == 0 {
            continue;
        }
        let (title, body) = crate::alerts::INTERACTION_PUSH;
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO push_outbox
                (outbox_key, kind, instance_id, interaction_id, tag_kind, tag_id, title, body,
                 url, created_at)
             VALUES (?1, 'lift', ?2, ?3, 'interaction', ?3, ?4, ?5, ?6, ?7)",
            params![
                format!("lift:{interaction_id}"),
                instance_id,
                interaction_id,
                title,
                body,
                crate::alerts::interaction_landing_url(&interaction_id),
                now
            ],
        )?;
        if inserted > 0 {
            note_outbox();
        }
        lifted += 1;
    }
    Ok(lifted)
}

// ─── reply-ready (§8.5) ─────────────────────────────────────────────────────

/// Whether `instance_id` holds the `address-owner` grant.
fn holds_address_owner(conn: &Connection, instance_id: &str) -> Result<bool, StoreError> {
    let grants: Option<String> = conn
        .query_row(
            "SELECT grants_json FROM instances WHERE id = ?1",
            params![instance_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    Ok(grants
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
        .is_some_and(|grants| grants.iter().any(|grant| grant == "address-owner")))
}

/// Whether `instance_id` is the live `address-owner` chapter: it holds the
/// grant and is its lineage's current chapter, at the live generation and
/// unfenced.
fn live_address_owner(conn: &Connection, instance_id: &str) -> Result<bool, StoreError> {
    if !holds_address_owner(conn, instance_id)? {
        return Ok(false);
    }
    Ok(conn
        .query_row(
            "SELECT 1 FROM instances i
               JOIN lineages l ON l.current_instance_id = i.id
              WHERE i.id = ?1 AND i.generation = l.generation AND i.fenced_at IS NULL",
            params![instance_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// The command id of a root user input `event` delivered, if it is one.
fn root_input_command(event: &Value) -> Option<&str> {
    if event.get("kind").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let payload = event.get("payload")?;
    let root_user = payload.get("role").and_then(Value::as_str) == Some("user")
        && payload.get("operation").and_then(Value::as_str) == Some("open")
        && payload.get("parentToolCallId").is_none_or(Value::is_null);
    root_user
        .then(|| payload.get("commandId").and_then(Value::as_str))
        .flatten()
}

/// Fold one appended journal event for reply-ready, in the append
/// transaction: each Human-origin input arms one tracker on the chapter; the
/// settled end of a turn (`turn_ended`) takes the tracker of the input that
/// turn answered and, on the live `address-owner` chapter, queues "Main
/// replied: <first line of the final message>". A turn end that names its
/// turn (`relatedIds.completedTurn`) takes only that input's tracker, so a
/// Human input that lands mid-turn waits for the turn that answers it; an
/// end that names no turn takes every armed tracker. A turn reached only by
/// Hub or Agent input arms nothing and pushes nothing.
pub(crate) fn fold_reply(
    conn: &Connection,
    instance_id: &str,
    event: &Value,
    seq: i64,
    turn_ended: bool,
) -> Result<(), StoreError> {
    if let Some(command_id) = root_input_command(event)
        && crate::root_turn::input_is_human(conn, instance_id, command_id)?
        && holds_address_owner(conn, instance_id)?
    {
        conn.execute(
            "INSERT OR IGNORE INTO push_reply_trackers
                (instance_id, command_id, armed_seq, armed_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![instance_id, command_id, seq, crate::config::now_rfc3339()],
        )?;
    }
    if !turn_ended {
        return Ok(());
    }
    let completed = event
        .pointer("/payload/relatedIds/completedTurn")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    let taken: Vec<i64> = match completed {
        Some(command_id) => conn
            .prepare(
                "DELETE FROM push_reply_trackers WHERE instance_id = ?1 AND command_id = ?2
                 RETURNING armed_seq",
            )?
            .query_map(params![instance_id, command_id], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?,
        None => conn
            .prepare("DELETE FROM push_reply_trackers WHERE instance_id = ?1 RETURNING armed_seq")?
            .query_map(params![instance_id], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?,
    };
    let Some(armed_seq) = taken.into_iter().min() else {
        return Ok(());
    };
    if !live_address_owner(conn, instance_id)? {
        return Ok(());
    }
    let line = final_message_line(conn, instance_id, armed_seq, seq)?;
    let title = if line.is_empty() {
        "Main replied".to_owned()
    } else {
        format!("Main replied: {line}")
    };
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO push_outbox
            (outbox_key, kind, instance_id, interaction_id, tag_kind, tag_id, title, body, url,
             created_at)
         VALUES (?1, 'reply', ?2, NULL, 'instance', ?2, ?3, ?4, ?5, ?6)",
        params![
            format!("reply:{instance_id}:{seq}"),
            instance_id,
            title,
            "Open Main to read the reply.",
            format!("/s/{instance_id}"),
            crate::config::now_rfc3339()
        ],
    )?;
    if inserted > 0 {
        note_outbox();
    }
    Ok(())
}

/// Text of a message's text blocks.
fn blocks_text(blocks: Option<&Value>) -> String {
    blocks
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The first non-empty line of the turn's last root assistant message (after
/// the input that armed it at `after`, up to the turn end at `seq`),
/// truncated to [`REPLY_LINE_CHARS`]. Empty when the turn wrote none.
fn final_message_line(
    conn: &Connection,
    instance_id: &str,
    after: i64,
    seq: i64,
) -> Result<String, StoreError> {
    let rows: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT payload_json FROM journal WHERE instance_id = ?1 AND seq > ?2 AND seq <= ?3
              ORDER BY seq DESC LIMIT ?4",
        )?;
        stmt.query_map(params![instance_id, after, seq, REPLY_SCAN_ROWS], |row| {
            row.get(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    // Ascending, folding each assistant message's mutations by id.
    let mut last: Option<(String, String)> = None;
    for raw in rows.iter().rev() {
        let Ok(event) = serde_json::from_str::<Value>(raw) else {
            continue;
        };
        if event.get("kind").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let Some(payload) = event.get("payload") else {
            continue;
        };
        if payload.get("role").and_then(Value::as_str) != Some("assistant")
            || payload
                .get("parentToolCallId")
                .is_some_and(|parent| !parent.is_null())
        {
            continue;
        }
        let id = payload
            .get("messageId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let text = blocks_text(payload.get("blocks"));
        let operation = payload
            .get("operation")
            .and_then(Value::as_str)
            .unwrap_or("open");
        match &mut last {
            Some((known, existing)) if *known == id => match operation {
                "append" => existing.push_str(&text),
                "close" => {}
                _ => *existing = text,
            },
            _ => last = Some((id, text)),
        }
    }
    let text = last.map(|(_, text)| text).unwrap_or_default();
    Ok(first_line(&text))
}

/// The first non-empty line of `text`, credential-shaped content redacted
/// (item 6: a push body never carries a secret), truncated with an ellipsis.
fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let line = crate::digest::redact_secrets(line);
    let mut out: String = line.chars().take(REPLY_LINE_CHARS).collect();
    if line.chars().count() > REPLY_LINE_CHARS {
        out.push('…');
    }
    out
}

// ─── the outbox ─────────────────────────────────────────────────────────────

/// In-process outbox state: drives are serialized, so one row is never sent
/// twice at once.
#[derive(Clone, Default)]
pub(crate) struct OutboxState {
    drive: Arc<tokio::sync::Mutex<()>>,
    /// Test seam: skip marking this many sent rows delivered (a crash
    /// between send and mark).
    #[cfg(any(test, feature = "test-faults"))]
    skip_marks: Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(any(test, feature = "test-faults"))]
impl OutboxState {
    /// Arm: the next `count` delivered rows are not marked delivered.
    pub(crate) fn skip_next_marks(&self, count: usize) {
        self.skip_marks
            .store(count, std::sync::atomic::Ordering::SeqCst);
    }
}

impl OutboxState {
    #[cfg(any(test, feature = "test-faults"))]
    fn skip_mark(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.skip_marks
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
    }

    #[cfg(not(any(test, feature = "test-faults")))]
    fn skip_mark(&self) -> bool {
        false
    }
}

/// One unsent outbox row.
#[derive(Debug)]
struct OutboxRow {
    key: String,
    kind: String,
    instance_id: String,
    interaction_id: Option<String>,
    tag: PushTag,
    title: String,
    body: String,
    url: String,
}

/// Send every undelivered outbox row with its own tag, then mark it
/// delivered. A lift row whose interaction is no longer pending is moot and
/// marked without a push. A failed fanout leaves the row for the next drive
/// (the tick, or the boot drive after a Hub restart); a crash between send
/// and mark resends it, and the tag replaces the first notification.
pub(crate) async fn drive_outbox(state: &AppState) {
    let drive = state.outbox.drive.clone();
    let _drive = drive.lock().await;
    send_outbox(state).await;
}

/// The tick's retry: skipped while another drive runs (that drive, or the
/// next tick, sends whatever it did not read).
pub(crate) async fn drive_outbox_if_idle(state: &AppState) {
    let drive = state.outbox.drive.clone();
    let Ok(_drive) = drive.try_lock() else {
        return;
    };
    send_outbox(state).await;
}

async fn send_outbox(state: &AppState) {
    let rows = match state
        .store
        .read("push_outbox_unsent", |conn| {
            let mut stmt = conn.prepare(
                "SELECT outbox_key, kind, instance_id, interaction_id, tag_kind, tag_id, title,
                        body, url
                   FROM push_outbox WHERE recipients IS NULL ORDER BY created_at, outbox_key",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    let tag_kind: String = row.get(4)?;
                    let tag_id: String = row.get(5)?;
                    Ok(OutboxRow {
                        key: row.get(0)?,
                        kind: row.get(1)?,
                        instance_id: row.get(2)?,
                        interaction_id: row.get(3)?,
                        tag: if tag_kind == "interaction" {
                            PushTag::Interaction { id: tag_id }
                        } else {
                            PushTag::Instance { id: tag_id }
                        },
                        title: row.get(6)?,
                        body: row.get(7)?,
                        url: row.get(8)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    {
        Ok(rows) => rows,
        Err(_) => {
            tracing::warn!(category = "outbox", "could not read the push outbox");
            return;
        }
    };
    for row in rows {
        let moot = match &row.interaction_id {
            Some(interaction_id) => !interaction_pending(state, interaction_id).await,
            None => false,
        };
        let recipients = if moot {
            Some(0)
        } else {
            let category = if row.kind == "lift" { "lift" } else { "reply" };
            crate::alerts::fanout(
                state,
                category,
                &row.instance_id,
                &row.title,
                &row.body,
                row.tag.clone(),
                row.url.clone(),
            )
            .await
            .recipients()
        };
        let Some(recipients) = recipients else {
            tracing::warn!(category = %row.kind, "outbox push failed; it stays undelivered");
            continue;
        };
        if state.outbox.skip_mark() {
            continue;
        }
        let key = row.key.clone();
        if state
            .store
            .run_named("push_outbox_sent", move |conn| {
                conn.execute(
                    "UPDATE push_outbox SET sent_at = ?2, recipients = ?3
                      WHERE outbox_key = ?1 AND recipients IS NULL",
                    params![
                        key,
                        crate::config::now_rfc3339(),
                        i64::try_from(recipients).unwrap_or(i64::MAX)
                    ],
                )?;
                Ok(())
            })
            .await
            .is_err()
        {
            tracing::warn!(category = %row.kind, "could not mark an outbox push delivered; it is resent");
        }
    }
}

async fn interaction_pending(state: &AppState, interaction_id: &str) -> bool {
    let id = interaction_id.to_owned();
    state
        .store
        .read("push_outbox_interaction_state", move |conn| {
            Ok(conn
                .query_row(
                    "SELECT 1 FROM interactions WHERE id = ?1 AND state = 'pending'",
                    params![id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some())
        })
        .await
        .unwrap_or(true)
}

// ─── the push log ───────────────────────────────────────────────────────────

/// One fanout, as the push log records it.
#[derive(Debug, Default)]
pub(crate) struct LogEntry {
    pub category: &'static str,
    pub tag: String,
    pub instance_id: String,
    pub outcome: &'static str,
    pub sent: usize,
    pub gone: usize,
    pub failed: usize,
    pub delivered_devices: Vec<String>,
    pub suppressed_devices: Vec<String>,
}

/// Record one fanout in the push log (best effort: a failed write is logged
/// and the push stands).
pub(crate) async fn log_push(state: &AppState, entry: LogEntry) {
    let category = entry.category;
    if state
        .store
        .run_named("push_log", move |conn| {
            let count = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
            conn.execute(
                "INSERT INTO push_log
                    (at, category, tag, instance_id, outcome, sent, gone, failed, suppressed,
                     delivered_devices_json, suppressed_devices_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    crate::config::now_rfc3339(),
                    entry.category,
                    entry.tag,
                    entry.instance_id,
                    entry.outcome,
                    count(entry.sent),
                    count(entry.gone),
                    count(entry.failed),
                    count(entry.suppressed_devices.len()),
                    serde_json::to_string(&entry.delivered_devices)?,
                    serde_json::to_string(&entry.suppressed_devices)?,
                ],
            )?;
            conn.execute(
                "DELETE FROM push_log WHERE id <= (SELECT MAX(id) FROM push_log) - ?1",
                params![PUSH_LOG_KEEP],
            )?;
            Ok(())
        })
        .await
        .is_err()
    {
        tracing::warn!(category, "could not record a push log row");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reply_line_never_carries_a_credential() {
        // Split so no source line reads as a token (scripts/ci/secret-scan.py
        // scans this file too); the redactor sees the real shape at run time.
        let token = ["gh", "p_abcdefghijklmnopqrstuvwxyz0123456789"].concat();
        let line = first_line(&format!("Pushed with {token} as asked.\nmore"));
        assert!(!line.contains(&token), "{line}");
        assert!(
            line.starts_with("Pushed with ") && line.ends_with(" as asked."),
            "{line}"
        );
    }

    #[test]
    fn the_first_line_is_trimmed_and_truncated() {
        assert_eq!(
            first_line("\n  Done: three files.  \nmore"),
            "Done: three files."
        );
        let long = "x".repeat(REPLY_LINE_CHARS + 5);
        let line = first_line(&long);
        assert_eq!(line.chars().count(), REPLY_LINE_CHARS + 1);
        assert!(line.ends_with('…'));
        assert_eq!(first_line(""), "");
    }

    #[test]
    fn only_root_user_inputs_with_a_command_arm_a_reply() {
        let input = |role: &str, parent: Value, command: Value| {
            serde_json::json!({"kind": "message", "payload": {
                "role": role, "operation": "open", "parentToolCallId": parent,
                "commandId": command}})
        };
        assert_eq!(
            root_input_command(&input("user", Value::Null, "cmd_1".into())),
            Some("cmd_1")
        );
        assert_eq!(
            root_input_command(&input("assistant", Value::Null, "cmd_1".into())),
            None
        );
        assert_eq!(
            root_input_command(&input("user", "tc_1".into(), "cmd_1".into())),
            None
        );
        assert_eq!(
            root_input_command(&input("user", Value::Null, Value::Null)),
            None
        );
    }

    /// Structural: every §8.3 cause calls the one transition function from
    /// the place that causes it (directly, or through its `Store` wrapper
    /// here).
    #[test]
    fn every_transition_cause_calls_the_one_function() {
        let sources = [
            ("fence.rs", include_str!("fence.rs"), "Transition::Fence"),
            (
                "restart.rs",
                include_str!("restart.rs"),
                "push_supervisor_transition",
            ),
            (
                "store.rs",
                include_str!("store.rs"),
                "Transition::ChapterEnded",
            ),
            ("store.rs", include_str!("store.rs"), "Transition::LinkLost"),
            ("store.rs", include_str!("store.rs"), "Transition::HostLost"),
            ("lib.rs", include_str!("lib.rs"), "push_boot_transition"),
            ("ws.rs", include_str!("ws.rs"), "push_hello_transition"),
        ];
        for (file, source, call) in sources {
            assert!(
                source.contains(call),
                "{file} must reach the transition via {call}"
            );
        }
        // The WS session end and the SSH carrier (loss and host removal) both
        // reach the link-loss cause through `mark_host_offline_rows`.
        let ws = include_str!("ws.rs");
        assert!(ws.contains("mark_host_offline_for_link") && ws.contains("mark_host_offline("));
        assert!(include_str!("ssh_hosts.rs").contains("mark_host_offline("));
        let store = include_str!("store.rs");
        let start = store
            .find("fn mark_host_offline_rows")
            .expect("mark_host_offline_rows");
        let body = &store[start..start + store[start..].find("\n}\n").expect("its end")];
        assert!(
            body.contains("Transition::LinkLost"),
            "mark_host_offline_rows calls the link-loss transition"
        );
        for wrapper in ["mark_host_offline(", "mark_host_offline_for_link("] {
            let at = store
                .find(&format!("pub async fn {wrapper}"))
                .expect(wrapper);
            let body = &store[at..at + store[at..].find("\n    }\n").expect("its end")];
            assert!(
                body.contains("mark_host_offline_rows"),
                "{wrapper} writes the rows"
            );
        }
    }
}
