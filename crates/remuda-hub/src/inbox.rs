//! The Hub inbox of facts (cb-inbox-facts; D-057 §10, D-060 §10.1).
//!
//! A fact is a durable row for a *subscriber lineage*: something it may act
//! on happened to a subject it is responsible for. This module writes and
//! keeps those rows; delivering them (batching, the idle gate, the owner-push
//! fallback, the paused hold) is `cb-inbox-deliver`'s, which reads the table
//! through [`Store::inbox_page`] and stamps `batch_id`.
//!
//! Tables (idempotent DDL, so no `user_version` rung; they live in
//! `hub.sqlite`, which `scripts/hub-backup.sh` carries whole):
//!
//! * `inbox_facts`: one row per recorded fact. `dedupe_key` is unique in its
//!   subscriber's inbox.
//! * `inbox_occurrences`: one row per occurrence of a fact, recorded or kept
//!   out by the cap, for the rolling 24 h count per
//!   `(subscriber, subject, kind)` (§10.1 item 2). It survives a restart.
//! * `inbox_subscriptions`: the creator's opt-in and the subscriber's
//!   per-kind mutes (§10, Defaults).
//! * `inbox_trackers`: per-instance journal-derived state (the run
//!   generation and the turns it maps; tool calls in flight).
//! * `inbox_sources`: state changes SQLite triggers queue in the writing
//!   transaction (a process end, a gate job's terminal state), turned into
//!   facts on the writer thread right after that job.
//! * `inbox_host_links`: the last host link state the inbox announced.
//! * `inbox_supply_parks`: children parked on a cooling supply window.
//!
//! Facts are written on the writer thread from journal appends and the
//! Hub's own committed state changes, never from polling a Node or a screen.
//! Facts entered by time passing (a supply window resetting, a host staying
//! offline past the grace, a stall, a wall budget) are due-time evaluations
//! over those durable rows on the scheduler tick, each claimed once by its
//! key.

use crate::AppState;
use crate::agent_scope::{authenticated_caller, origin};
use crate::auth::require_origin;
use crate::config::now_rfc3339;
use crate::error::HubError;
use crate::http::map_store;
use crate::store::{Store, StoreError};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use remuda_protocol::{
    DEFAULT_FACT_OCCURRENCE_CAP, FACT_OCCURRENCE_WINDOW_HOURS, INBOX_PAGE_DEFAULT_LIMIT,
    INBOX_PAGE_MAX_LIMIT, InboxFact, InboxFactKind, InboxOccurrence, InboxPage, InboxSettings,
    InboxSettingsPatch, InputOrigin,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Days an occurrence row is kept: well past the 24 h window, so an
/// out-of-order occurrence still counts against the right window.
const OCCURRENCE_RETENTION_DAYS: i64 = 7;
/// Characters of worker- or tool-produced text a payload carries.
const FACT_TEXT_MAX_CHARS: usize = 200;

/// Idempotent DDL; no `user_version` rung.
pub(crate) fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS inbox_facts (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            subscriber_lineage_id TEXT NOT NULL,
            kind TEXT NOT NULL,
            dedupe_key TEXT NOT NULL,
            subject_instance_id TEXT,
            run_generation INTEGER NOT NULL DEFAULT 0,
            payload_json TEXT NOT NULL,
            state_entered_at TEXT NOT NULL,
            created_at TEXT NOT NULL,
            batch_id TEXT,
            UNIQUE (subscriber_lineage_id, dedupe_key)
         );
         CREATE INDEX IF NOT EXISTS inbox_facts_unbatched
            ON inbox_facts(subscriber_lineage_id, id) WHERE batch_id IS NULL;
         CREATE TABLE IF NOT EXISTS inbox_occurrences (
            subscriber_lineage_id TEXT NOT NULL,
            dedupe_key TEXT NOT NULL,
            subject TEXT NOT NULL,
            kind TEXT NOT NULL,
            state_entered_at TEXT NOT NULL,
            recorded INTEGER NOT NULL,
            PRIMARY KEY (subscriber_lineage_id, dedupe_key)
         );
         CREATE INDEX IF NOT EXISTS inbox_occurrences_window
            ON inbox_occurrences(subscriber_lineage_id, subject, kind, state_entered_at);
         CREATE TABLE IF NOT EXISTS inbox_subscriptions (
            lineage_id TEXT PRIMARY KEY,
            opt_in INTEGER NOT NULL DEFAULT 0,
            muted_kinds_json TEXT NOT NULL DEFAULT '[]',
            updated_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS inbox_trackers (
            instance_id TEXT PRIMARY KEY,
            tracker_json TEXT NOT NULL,
            updated_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS inbox_host_links (
            host_id TEXT PRIMARY KEY,
            online INTEGER NOT NULL,
            changed_at TEXT NOT NULL,
            lost_at TEXT
         );
         CREATE TABLE IF NOT EXISTS inbox_supply_parks (
            instance_id TEXT NOT NULL,
            profile_id TEXT NOT NULL,
            family TEXT NOT NULL,
            run_generation INTEGER NOT NULL,
            resets_at TEXT NOT NULL,
            parked_at TEXT NOT NULL,
            PRIMARY KEY (instance_id, profile_id, family)
         );
         CREATE INDEX IF NOT EXISTS inbox_supply_parks_due ON inbox_supply_parks(resets_at);",
    )?;
    migrate_sources(conn)
}

// ── settings ───────────────────────────────────────────────────────────────

/// The stored settings of `lineage_id`; defaults when it has none.
pub(crate) fn load_settings(
    conn: &Connection,
    lineage_id: &str,
) -> Result<InboxSettings, StoreError> {
    let row: Option<(bool, String)> = conn
        .query_row(
            "SELECT opt_in, muted_kinds_json FROM inbox_subscriptions WHERE lineage_id = ?1",
            params![lineage_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((opt_in, muted)) = row else {
        return Ok(InboxSettings::default());
    };
    // Unknown spellings (a kind a later build removed) are dropped, never an
    // error: a settings row must not stop facts from being written.
    let muted_kinds = serde_json::from_str::<Vec<String>>(&muted)
        .unwrap_or_default()
        .iter()
        .filter_map(|raw| InboxFactKind::parse(raw))
        .collect();
    Ok(InboxSettings {
        opt_in,
        muted_kinds,
    })
}

fn save_settings(
    conn: &Connection,
    lineage_id: &str,
    settings: &InboxSettings,
) -> Result<(), StoreError> {
    let mut muted: Vec<&str> = settings
        .muted_kinds
        .iter()
        .map(|kind| kind.as_str())
        .collect();
    muted.sort_unstable();
    muted.dedup();
    conn.execute(
        "INSERT INTO inbox_subscriptions (lineage_id, opt_in, muted_kinds_json, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(lineage_id) DO UPDATE SET
            opt_in = excluded.opt_in,
            muted_kinds_json = excluded.muted_kinds_json,
            updated_at = excluded.updated_at",
        params![
            lineage_id,
            settings.opt_in,
            serde_json::to_string(&muted)?,
            now_rfc3339()
        ],
    )?;
    Ok(())
}

// ── reads ──────────────────────────────────────────────────────────────────

/// One stored row. `None` for a kind this build does not know (a row a newer
/// build wrote): rows are written from [`InboxFactKind`] only, and a reader
/// never relabels one as another kind.
fn fact_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<InboxFact>> {
    let kind: String = row.get(2)?;
    let Some(kind) = InboxFactKind::parse(&kind) else {
        return Ok(None);
    };
    let payload: String = row.get(6)?;
    Ok(Some(InboxFact {
        id: row.get(0)?,
        lineage_id: row.get(1)?,
        kind,
        dedupe_key: row.get(3)?,
        subject_instance_id: row.get(4)?,
        run_generation: row.get(5)?,
        payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
        state_entered_at: row.get(7)?,
        created_at: row.get(8)?,
        batch_id: row.get(9)?,
    }))
}

/// One page of `lineage_id`'s inbox: rows with `id > after`, oldest first.
pub(crate) fn page(
    conn: &Connection,
    lineage_id: &str,
    after: i64,
    limit: u32,
) -> Result<InboxPage, StoreError> {
    let limit = limit.clamp(1, INBOX_PAGE_MAX_LIMIT);
    let mut stmt = conn.prepare_cached(
        "SELECT id, subscriber_lineage_id, kind, dedupe_key, subject_instance_id,
                run_generation, payload_json, state_entered_at, created_at, batch_id
           FROM inbox_facts
          WHERE subscriber_lineage_id = ?1 AND id > ?2
          ORDER BY id ASC
          LIMIT ?3",
    )?;
    let rows = stmt
        .query_map(params![lineage_id, after, i64::from(limit) + 1], |row| {
            Ok((row.get::<_, i64>(0)?, fact_from_row(row)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let more = rows.len() > limit as usize;
    // The cursor advances over every row read, a skipped unknown kind too.
    let next_after = rows[..rows.len().min(limit as usize)]
        .last()
        .map_or(after, |(id, _)| *id);
    let items = rows
        .into_iter()
        .take(limit as usize)
        .filter_map(|(_, fact)| fact)
        .collect();
    Ok(InboxPage {
        lineage_id: lineage_id.to_owned(),
        items,
        next_after,
        more,
        settings: load_settings(conn, lineage_id)?,
    })
}

impl Store {
    /// `GET /v1/lineages/{id}/inbox`: one page, read on the reader pool.
    pub(crate) async fn inbox_page(
        &self,
        lineage_id: String,
        after: i64,
        limit: u32,
    ) -> Result<InboxPage, StoreError> {
        self.read("inbox_page", move |conn| {
            page(conn, &lineage_id, after, limit)
        })
        .await
    }

    /// `PATCH /v1/lineages/{id}/inbox`: absent fields keep their value.
    pub(crate) async fn patch_inbox_settings(
        &self,
        lineage_id: String,
        patch: InboxSettingsPatch,
    ) -> Result<InboxSettings, StoreError> {
        self.run_named("patch_inbox_settings", move |conn| {
            let tx = crate::store::immediate_tx(conn)?;
            let mut settings = load_settings(&tx, &lineage_id)?;
            if let Some(opt_in) = patch.opt_in {
                settings.opt_in = opt_in;
            }
            if let Some(muted) = patch.muted_kinds {
                settings.muted_kinds = muted;
            }
            save_settings(&tx, &lineage_id, &settings)?;
            let settings = load_settings(&tx, &lineage_id)?;
            tx.commit()?;
            Ok(settings)
        })
        .await
    }
}

// ── HTTP ───────────────────────────────────────────────────────────────────

pub(crate) fn routes() -> Router<AppState> {
    Router::new().route(
        "/v1/lineages/{id}/inbox",
        get(read_inbox).patch(patch_inbox),
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InboxQuery {
    #[serde(default)]
    after: Option<i64>,
    #[serde(default)]
    limit: Option<u32>,
}

/// Who may use the inbox routes of `id`, resolved to the lineage id.
///
/// The lineage read predicate: a Human device reads any lineage (and may name
/// any chapter of it); an Agent only its own lineage, by its canonical id.
/// The decision is taken on how the credential AUTHENTICATED as well: a Bot
/// relays other people's words and is refused, whatever chapter it names
/// through `x-remuda-instance-id`.
async fn inbox_caller(
    state: &AppState,
    headers: &HeaderMap,
    id: String,
) -> Result<(InputOrigin, String), HubError> {
    require_origin(headers, &state.config)?;
    let (authenticated, device) = authenticated_caller(state, headers).await?;
    if origin(&authenticated) == InputOrigin::Bot {
        return Err(HubError::Forbidden);
    }
    match origin(&device) {
        InputOrigin::Agent => {
            let bound = device.instance_id.ok_or(HubError::Forbidden)?;
            let own = state
                .store
                .lineage_id_for_instance(bound)
                .await
                .map_err(map_store)?
                .ok_or(HubError::Forbidden)?;
            if own != id {
                return Err(HubError::Forbidden);
            }
            Ok((InputOrigin::Agent, own))
        }
        InputOrigin::Human => {
            let lineage = state
                .store
                .lineage_id_for_instance(id)
                .await
                .map_err(map_store)?
                .ok_or(HubError::NotFound)?;
            Ok((InputOrigin::Human, lineage))
        }
        _ => Err(HubError::Forbidden),
    }
}

async fn read_inbox(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<InboxQuery>,
) -> Result<Json<InboxPage>, HubError> {
    let (_, lineage_id) = inbox_caller(&state, &headers, id).await?;
    let after = query.after.unwrap_or(0);
    if after < 0 {
        return Err(HubError::BadRequest("after must not be negative".into()));
    }
    let limit = query.limit.unwrap_or(INBOX_PAGE_DEFAULT_LIMIT);
    if limit == 0 || limit > INBOX_PAGE_MAX_LIMIT {
        return Err(HubError::BadRequest(format!(
            "limit must be between 1 and {INBOX_PAGE_MAX_LIMIT}"
        )));
    }
    Ok(Json(
        state
            .store
            .inbox_page(lineage_id, after, limit)
            .await
            .map_err(map_store)?,
    ))
}

/// The creator's opt-in is the owner's to give (§10, Defaults); a lineage
/// may mute kinds for itself, and so may the owner.
async fn patch_inbox(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(patch): Json<InboxSettingsPatch>,
) -> Result<Json<InboxSettings>, HubError> {
    let (caller, lineage_id) = inbox_caller(&state, &headers, id).await?;
    if patch.opt_in.is_some() && caller != InputOrigin::Human {
        return Err(HubError::Forbidden);
    }
    Ok(Json(
        state
            .store
            .patch_inbox_settings(lineage_id, patch)
            .await
            .map_err(map_store)?,
    ))
}

// ── time ───────────────────────────────────────────────────────────────────

/// `raw` as the Hub's fixed-width UTC RFC3339 shape (ms, `Z`), so stored
/// times compare lexicographically and a key built from one is the same
/// string whichever offset the source wrote.
pub(crate) fn canonical_at(raw: &str) -> Option<String> {
    parse_at(raw).map(crate::config::rfc3339)
}

fn parse_at(raw: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(raw.trim(), &Rfc3339)
        .ok()
        .map(|at| at.to_offset(time::UtcOffset::UTC))
}

/// `at` shifted by `delta`, in the canonical shape. An unparsable `at`
/// (never written by this module) comes back unchanged.
fn shifted(at: &str, delta: time::Duration) -> String {
    parse_at(at).map_or_else(|| at.to_owned(), |at| crate::config::rfc3339(at + delta))
}

// ── subscribers ────────────────────────────────────────────────────────────

/// The lineage that receives facts about `subject`: its parent's lineage,
/// recorded at write time (`cb-inbox-deliver` re-checks it when a batch
/// forms). `None` without a parent, and when the parent edge leads back into
/// the subject's own lineage: a lineage never receives facts about its own
/// chapters (§10.1 item 4).
pub(crate) fn subscriber_of(
    conn: &Connection,
    subject: &str,
) -> Result<Option<String>, StoreError> {
    let Some(own) = crate::store::lineage_id_of(conn, subject)? else {
        return Ok(None);
    };
    let Some(parent) = crate::store::parent_lineage_of(conn, subject)? else {
        return Ok(None);
    };
    Ok((parent != own).then_some(parent))
}

/// Whether `lineage_id` receives facts of `kind` (§10, Defaults): a
/// continuity lineage or an agent-created coordinator does; a Human-created
/// instance without continuity only when its creator opted in. A muted kind
/// is never received.
fn receives(conn: &Connection, lineage_id: &str, kind: InboxFactKind) -> Result<bool, StoreError> {
    let settings = load_settings(conn, lineage_id)?;
    if settings.muted_kinds.contains(&kind) {
        return Ok(false);
    }
    if settings.opt_in {
        return Ok(true);
    }
    // D-057 §5: a lineage row exists for every continuity instance.
    let continuity: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM lineages WHERE lineage_id = ?1)",
        params![lineage_id],
        |row| row.get(0),
    )?;
    if continuity {
        return Ok(true);
    }
    // An Agent created it (the create spec carries the stamped origin), and
    // it is a parent: the subject is its child.
    let agent_created: bool = conn.query_row(
        "SELECT EXISTS (
            SELECT 1 FROM instances
             WHERE COALESCE(lineage_id, id) = ?1
               AND json_extract(spec_json, '$.origin') = 'agent')",
        params![lineage_id],
        |row| row.get(0),
    )?;
    Ok(agent_created)
}

/// The single project of `instance_id`'s scope, whose policy holds the cap
/// and the stall windows.
fn project_of(conn: &Connection, instance_id: &str) -> Result<Option<String>, StoreError> {
    let scope: Option<String> = conn
        .query_row(
            "SELECT scope_json FROM instances WHERE id = ?1",
            params![instance_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    Ok(scope
        .and_then(|raw| serde_json::from_str::<remuda_protocol::InstanceScope>(&raw).ok())
        .and_then(|scope| scope.single_project_id().map(|id| id.as_id().to_string())))
}

/// One integer of a project's `policy.configurable`; `None` when the project,
/// the field or a number is absent.
fn project_policy_number(
    conn: &Connection,
    project_id: Option<&str>,
    field: &str,
) -> Result<Option<i64>, StoreError> {
    let Some(project_id) = project_id else {
        return Ok(None);
    };
    let raw: Option<String> = conn
        .query_row(
            "SELECT doc_json FROM projects WHERE id = ?1",
            params![project_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(raw
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|doc| {
            doc.pointer(&format!("/policy/configurable/{field}"))
                .and_then(Value::as_i64)
        }))
}

fn occurrence_cap(conn: &Connection, project_id: Option<&str>) -> Result<i64, StoreError> {
    Ok(
        project_policy_number(conn, project_id, "factOccurrenceCap")?
            .filter(|cap| *cap >= 0)
            .unwrap_or(DEFAULT_FACT_OCCURRENCE_CAP),
    )
}

// ── recording ──────────────────────────────────────────────────────────────

/// A fact about to be recorded for one subscriber.
#[derive(Debug, Clone)]
pub(crate) struct Fact {
    pub(crate) kind: InboxFactKind,
    /// The instance the fact is about, when it is about one.
    pub(crate) subject_instance_id: Option<String>,
    /// The key's subject segment: the instance, or the interaction, gate
    /// job, host or task the fact names when one instance can have several.
    pub(crate) key_subject: String,
    /// What the occurrence cap counts per, beside subscriber and kind: the
    /// subject instance, or the host / gate job a fact without one names.
    pub(crate) cap_subject: String,
    /// The subject's run generation the fact is about; 0 when not a run.
    pub(crate) run: i64,
    /// When the subject entered the state (canonical shape).
    pub(crate) entered_at: String,
    /// The project whose policy sets the cap.
    pub(crate) project_id: Option<String>,
    /// Kind-specific detail (a JSON object).
    pub(crate) payload: Value,
}

/// What [`record`] did with a fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// A new inbox row.
    Recorded(i64),
    /// The same key was already seen for this subscriber (a replay).
    Duplicate,
    /// Counted, but the subscriber already holds `cap` of these in the window.
    Capped,
    /// The subscriber does not receive this kind (default, opt-in or mute).
    NotReceived,
}

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Self::Recorded(_) => "recorded",
            Self::Duplicate => "duplicate",
            Self::Capped => "capped",
            Self::NotReceived => "not-received",
        }
    }
}

/// `kind:subject:run:stateEnteredAt` (§10.1 items 1 and 3).
pub(crate) fn dedupe_key(kind: InboxFactKind, subject: &str, run: i64, entered_at: &str) -> String {
    format!("{}:{subject}:{run}:{entered_at}", kind.as_str())
}

/// Record `fact` for `subscriber` on the caller's connection (the writer's
/// transaction it belongs to). Idempotent per dedupe key; the occurrence is
/// counted before the cap is applied, so a capped occurrence still raises the
/// count the next recorded fact carries.
pub(crate) fn record(
    conn: &Connection,
    subscriber: &str,
    mut fact: Fact,
) -> Result<Outcome, StoreError> {
    let outcome = record_inner(conn, subscriber, &mut fact)?;
    tracing::debug!(
        kind = fact.kind.as_str(),
        outcome = outcome.label(),
        "inbox fact"
    );
    Ok(outcome)
}

fn record_inner(
    conn: &Connection,
    subscriber: &str,
    fact: &mut Fact,
) -> Result<Outcome, StoreError> {
    if !receives(conn, subscriber, fact.kind)? {
        return Ok(Outcome::NotReceived);
    }
    let key = dedupe_key(fact.kind, &fact.key_subject, fact.run, &fact.entered_at);
    let seen: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM inbox_occurrences
                         WHERE subscriber_lineage_id = ?1 AND dedupe_key = ?2)
             OR EXISTS (SELECT 1 FROM inbox_facts
                         WHERE subscriber_lineage_id = ?1 AND dedupe_key = ?2)",
        params![subscriber, key],
        |row| row.get(0),
    )?;
    if seen {
        return Ok(Outcome::Duplicate);
    }
    let cap = occurrence_cap(conn, fact.project_id.as_deref())?;
    let window_start = shifted(
        &fact.entered_at,
        -time::Duration::hours(FACT_OCCURRENCE_WINDOW_HOURS),
    );
    let (earlier, earlier_recorded): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(recorded), 0) FROM inbox_occurrences
          WHERE subscriber_lineage_id = ?1 AND subject = ?2 AND kind = ?3
            AND state_entered_at > ?4 AND state_entered_at <= ?5",
        params![
            subscriber,
            fact.cap_subject,
            fact.kind.as_str(),
            window_start,
            fact.entered_at
        ],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let admitted = earlier_recorded < cap;
    conn.execute(
        "INSERT INTO inbox_occurrences
            (subscriber_lineage_id, dedupe_key, subject, kind, state_entered_at, recorded)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            subscriber,
            key,
            fact.cap_subject,
            fact.kind.as_str(),
            fact.entered_at,
            admitted
        ],
    )?;
    // Occurrences far outside any window a new fact can still land in are
    // dropped; the inbox row itself keeps the key unique forever.
    conn.execute(
        "DELETE FROM inbox_occurrences
          WHERE subscriber_lineage_id = ?1 AND subject = ?2 AND kind = ?3
            AND state_entered_at < ?4",
        params![
            subscriber,
            fact.cap_subject,
            fact.kind.as_str(),
            shifted(
                &fact.entered_at,
                -time::Duration::days(OCCURRENCE_RETENTION_DAYS)
            )
        ],
    )?;
    if !admitted {
        return Ok(Outcome::Capped);
    }
    let occurrence = InboxOccurrence {
        count: earlier + 1,
        capped: earlier - earlier_recorded,
        cap,
        window_hours: FACT_OCCURRENCE_WINDOW_HOURS,
    };
    if !fact.payload.is_object() {
        fact.payload = json!({});
    }
    fact.payload["occurrence"] = serde_json::to_value(&occurrence)?;
    conn.execute(
        "INSERT INTO inbox_facts
            (subscriber_lineage_id, kind, dedupe_key, subject_instance_id, run_generation,
             payload_json, state_entered_at, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            subscriber,
            fact.kind.as_str(),
            key,
            fact.subject_instance_id,
            fact.run,
            fact.payload.to_string(),
            fact.entered_at,
            now_rfc3339()
        ],
    )?;
    Ok(Outcome::Recorded(conn.last_insert_rowid()))
}

/// Record a fact about one instance for that instance's subscriber, if it
/// has one (§10.1 item 4).
fn record_about(conn: &Connection, instance_id: &str, fact: Fact) -> Result<Outcome, StoreError> {
    match subscriber_of(conn, instance_id)? {
        Some(subscriber) => record(conn, &subscriber, fact),
        None => Ok(Outcome::NotReceived),
    }
}

/// A fact about `instance_id` itself (key and cap subject are the instance).
fn instance_fact(
    conn: &Connection,
    kind: InboxFactKind,
    instance_id: &str,
    run: i64,
    entered_at: String,
    payload: Value,
) -> Result<Fact, StoreError> {
    Ok(Fact {
        kind,
        subject_instance_id: Some(instance_id.to_owned()),
        key_subject: instance_id.to_owned(),
        cap_subject: instance_id.to_owned(),
        run,
        entered_at,
        project_id: project_of(conn, instance_id)?,
        payload,
    })
}

/// Free text from a worker or tool, bounded for a fact payload.
fn bounded_text(text: &str) -> String {
    text.trim().chars().take(FACT_TEXT_MAX_CHARS).collect()
}

// ── the per-instance run tracker ───────────────────────────────────────────

/// Root-turn commands remembered per instance, newest last.
const TRACKED_TURNS: usize = 32;

/// Journal-derived per-instance state the facts need beyond the instance row,
/// folded in the append transaction (`inbox_trackers.tracker_json`).
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Tracker {
    /// Run generation: the root turns the instance has started. A
    /// structured carrier counts its root `turn_started` records; a carrier
    /// without turn boundaries counts busy edges out of idle.
    #[serde(default)]
    run: i64,
    /// Highest run a turn fact has named. A late result of an earlier run
    /// never raises it past the run it belongs to, so the newer run's own
    /// end is still recorded.
    #[serde(default)]
    settled: i64,
    /// The carrier reports root turn boundaries.
    #[serde(default)]
    structured: bool,
    /// `(commandId, run)` of recent root turns started for an input.
    #[serde(default)]
    turns: Vec<(String, i64)>,
}

impl Tracker {
    fn run_of(&self, command_id: &str) -> Option<i64> {
        self.turns
            .iter()
            .rev()
            .find(|(id, _)| id == command_id)
            .map(|(_, run)| *run)
    }
}

fn load_tracker(conn: &Connection, instance_id: &str) -> Result<Tracker, StoreError> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT tracker_json FROM inbox_trackers WHERE instance_id = ?1",
            params![instance_id],
            |row| row.get(0),
        )
        .optional()?;
    // An unreadable row restarts the count rather than failing the append.
    Ok(raw
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default())
}

fn save_tracker(conn: &Connection, instance_id: &str, tracker: &Tracker) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO inbox_trackers (instance_id, tracker_json, updated_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(instance_id) DO UPDATE SET
            tracker_json = excluded.tracker_json, updated_at = excluded.updated_at",
        params![instance_id, serde_json::to_string(tracker)?, now_rfc3339()],
    )?;
    Ok(())
}

/// Drop the per-instance rows of a deleted instance (its recorded facts
/// stay in their subscribers' inboxes).
pub(crate) fn forget_instance(conn: &Connection, instance_id: &str) -> Result<(), StoreError> {
    conn.execute(
        "DELETE FROM inbox_trackers WHERE instance_id = ?1",
        params![instance_id],
    )?;
    conn.execute(
        "DELETE FROM inbox_supply_parks WHERE instance_id = ?1",
        params![instance_id],
    )?;
    Ok(())
}

/// The run generation `instance_id` is in now.
fn current_run(conn: &Connection, instance_id: &str) -> Result<i64, StoreError> {
    Ok(load_tracker(conn, instance_id)?.run)
}

// ── facts from a journal append ────────────────────────────────────────────

/// The instance row's state before an append's projections ran, for the
/// events that can produce a fact.
#[derive(Debug, Clone)]
pub(crate) struct Before {
    lifecycle: String,
    activity: String,
    /// The event requests an interaction the Hub already held (a re-sent
    /// request is not a new interaction).
    interaction_known: bool,
}

fn event_kind(event: &Value) -> &str {
    event
        .get("kind")
        .and_then(Value::as_str)
        .or_else(|| event.get("subtype").and_then(Value::as_str))
        .unwrap_or("")
}

/// The interaction a request event names (the same lookup the store's
/// interaction projection uses).
fn requested_interaction_id(event: &Value) -> Option<&str> {
    if !matches!(
        event_kind(event),
        "interaction.requested" | "interactionRequested"
    ) {
        return None;
    }
    event
        .get("interactionId")
        .and_then(Value::as_str)
        .or_else(|| {
            event
                .pointer("/payload/interactionId")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            event
                .pointer("/payload/interaction/id")
                .and_then(Value::as_str)
        })
        .filter(|id| !id.is_empty())
}

/// Events that can change what a fact reads; anything else (messages,
/// usage, artifacts) costs the append nothing here.
fn watches(event: &Value) -> bool {
    if matches!(
        event_kind(event),
        "lifecycle" | "interaction.requested" | "interactionRequested"
    ) {
        return true;
    }
    let (lifecycle, activity) = crate::store::derive_instance_state(event);
    lifecycle.is_some() || activity.is_some()
}

/// Read the instance row before an append's projections run. `None` when
/// the event cannot produce a fact.
pub(crate) fn before_append(
    conn: &Connection,
    instance_id: &str,
    event: &Value,
) -> Result<Option<Before>, StoreError> {
    if !watches(event) {
        return Ok(None);
    }
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT lifecycle, activity FROM instances WHERE id = ?1",
            params![instance_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((lifecycle, activity)) = row else {
        return Ok(None);
    };
    let interaction_known = match requested_interaction_id(event) {
        Some(id) => conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM interactions WHERE id = ?1)",
            params![id],
            |row| row.get(0),
        )?,
        None => false,
    };
    Ok(Some(Before {
        lifecycle,
        activity,
        interaction_known,
    }))
}

fn is_busy(activity: &str) -> bool {
    matches!(activity, "working" | "blocked" | "draining")
}

fn is_live(lifecycle: &str) -> bool {
    matches!(lifecycle, "running" | "ready")
}

fn knowledge_text(value: Option<&Value>) -> Option<&str> {
    let value = value?;
    value
        .as_str()
        .or_else(|| value.get("value").and_then(Value::as_str))
}

/// Fold one appended (not replayed) journal event into the facts it implies,
/// after every projection of the append ran, in the same transaction.
/// `turn_ended` is the projection's busy→idle verdict for this event.
pub(crate) fn fold_append(
    conn: &Connection,
    instance_id: &str,
    event: &Value,
    hub_now: &str,
    before: Before,
    turn_ended: bool,
) -> Result<(), StoreError> {
    let after: Option<(String, String)> = conn
        .query_row(
            "SELECT lifecycle, activity FROM instances WHERE id = ?1",
            params![instance_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((lifecycle, activity)) = after else {
        return Ok(());
    };
    // §10.1 item 1: journal time, so the key is the same after a restart.
    let entered_at = event
        .get("observedAt")
        .and_then(Value::as_str)
        .and_then(canonical_at)
        .or_else(|| canonical_at(hub_now))
        .unwrap_or_else(now_rfc3339);
    let payload = event.get("payload").unwrap_or(&Value::Null);
    let text = |pointer: &str| payload.pointer(pointer).and_then(Value::as_str);
    let root_turn = text("/type") == Some("native")
        && text("/topic") == Some("turn")
        && !crate::store::native_payload_is_subagent(payload);
    let native_name = text("/nativeName").unwrap_or("");

    let mut tracker = load_tracker(conn, instance_id)?;
    let original = tracker.clone();
    if root_turn && native_name == "turn_started" {
        tracker.run += 1;
        tracker.structured = true;
        if let Some(command_id) = text("/relatedIds/nativeClientMessageId") {
            if tracker.turns.len() >= TRACKED_TURNS {
                tracker.turns.remove(0);
            }
            tracker.turns.push((command_id.to_owned(), tracker.run));
        }
    } else if !tracker.structured && is_busy(&activity) && !is_busy(&before.activity) {
        tracker.run += 1;
    }

    if is_live(&lifecycle) && !is_live(&before.lifecycle) {
        let fact = instance_fact(
            conn,
            InboxFactKind::Started,
            instance_id,
            tracker.run,
            entered_at.clone(),
            json!({ "lifecycle": lifecycle, "from": before.lifecycle }),
        )?;
        record_about(conn, instance_id, fact)?;
    }

    let requested = requested_interaction_id(event);
    if let Some(interaction_id) = requested
        && !before.interaction_known
    {
        let row: Option<(String, String, bool)> = conn
            .query_row(
                "SELECT kind, state, blocking FROM interactions WHERE id = ?1",
                params![interaction_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((interaction_kind, state, blocking)) = row
            && state == "pending"
        {
            let routed =
                crate::interactions::routed_to_parent_conn(conn, instance_id, &interaction_kind)?;
            let mut fact = instance_fact(
                conn,
                InboxFactKind::Interaction,
                instance_id,
                tracker.run,
                entered_at.clone(),
                json!({
                    "interactionId": interaction_id,
                    "interactionKind": interaction_kind,
                    "blocking": blocking,
                    "routedToParent": routed,
                }),
            )?;
            // Several interactions can open at one instant.
            fact.key_subject = format!("{instance_id}/{interaction_id}");
            record_about(conn, instance_id, fact)?;
        }
    }

    if activity == "blocked" && before.activity != "blocked" && requested.is_none() {
        // Waiting without an interaction the Hub knows of (a screen-reporting
        // carrier's prompt); one that has one is the interaction fact.
        let pending: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM interactions
                             WHERE instance_id = ?1 AND state = 'pending')",
            params![instance_id],
            |row| row.get(0),
        )?;
        if !pending {
            let fact = instance_fact(
                conn,
                InboxFactKind::Waiting,
                instance_id,
                tracker.run,
                entered_at.clone(),
                json!({ "activity": activity }),
            )?;
            record_about(conn, instance_id, fact)?;
        }
    }

    let status = knowledge_text(payload.get("status"));
    let completed = text("/relatedIds/completedTurn");
    let settles_root = text("/relatedIds/settledRootTurn") == Some("true");
    let turn_fact = if root_turn && native_name == "result" && (completed.is_some() || settles_root)
    {
        // A structured result names its own turn: a late result of an
        // earlier run is that run's outcome, never the current one's.
        let run = completed
            .and_then(|command_id| tracker.run_of(command_id))
            .unwrap_or(tracker.run);
        Some((run, status == Some("error")))
    } else if turn_ended && tracker.settled < tracker.run {
        let failed = status == Some("error")
            || text("/relatedIds/outcome").is_some_and(|o| o.eq_ignore_ascii_case("failed"));
        Some((tracker.run, failed))
    } else {
        None
    };
    if let Some((run, failed)) = turn_fact {
        let mut detail = json!({
            "outcome": if failed { "failed" } else { "done" },
            "late": run < tracker.run,
            "currentRun": tracker.run,
        });
        if failed && let Some(error) = text("/relatedIds/lastError").map(bounded_text) {
            detail["error"] = json!(error);
        }
        let kind = if failed {
            InboxFactKind::TurnFailed
        } else {
            InboxFactKind::TurnDone
        };
        let fact = instance_fact(conn, kind, instance_id, run, entered_at.clone(), detail)?;
        record_about(conn, instance_id, fact)?;
        tracker.settled = tracker.settled.max(run);
    }

    if tracker != original {
        save_tracker(conn, instance_id, &tracker)?;
    }
    Ok(())
}

// ── facts from the Hub's own committed state changes ───────────────────────

/// Attempts at one source row before it is dropped as poison.
const MAX_SOURCE_ATTEMPTS: i64 = 5;
/// Source rows one drain call reads per transaction.
const SOURCE_BATCH: i64 = 32;
/// Transactions one drain call runs at most; the rest wait for the next job.
const SOURCE_BATCHES: usize = 8;
/// Gate job states a job never leaves.
const GATE_TERMINAL: [&str; 4] = ["passed", "failed", "landed", "canceled"];

/// The source queue's triggers, rebuilt on every open so a change to their
/// predicates reaches existing databases.
fn migrate_sources(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS inbox_sources (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            kind TEXT NOT NULL,
            subject TEXT NOT NULL,
            entered_at TEXT NOT NULL,
            attempts INTEGER NOT NULL DEFAULT 0
         );
         -- Every path that records a process end stamps `end_seen_at` once
         -- (restart.rs); the exit fact follows that one stamp.
         DROP TRIGGER IF EXISTS inbox_source_exit;
         CREATE TRIGGER inbox_source_exit AFTER UPDATE OF end_seen_at ON instances
         WHEN OLD.end_seen_at IS NULL AND NEW.end_seen_at IS NOT NULL
         BEGIN
           INSERT INTO inbox_sources (kind, subject, entered_at)
           VALUES ('exited', NEW.id, NEW.end_seen_at);
         END;
         DROP TRIGGER IF EXISTS inbox_source_gate;
         CREATE TRIGGER inbox_source_gate AFTER UPDATE OF state ON gate_jobs
         WHEN NEW.state IN ('passed', 'failed', 'landed', 'canceled')
          AND OLD.state NOT IN ('passed', 'failed', 'landed', 'canceled')
         BEGIN
           INSERT INTO inbox_sources (kind, subject, entered_at)
           VALUES ('gate', NEW.id,
                   COALESCE(json_extract(NEW.doc_json, '$.finishedAt'), NEW.updated_at));
         END;",
    )
}

/// Turn the source rows the triggers queued into facts. Runs on the writer
/// connection after every writer job (and once at open), so a state change
/// and its fact are one durable hand-off: the source row commits with the
/// change, and the fact commits with the row's removal. A row whose fact
/// cannot be written stays for the next job; one that fails
/// [`MAX_SOURCE_ATTEMPTS`] times is dropped and logged by error code.
pub(crate) fn drain_sources(conn: &mut Connection) {
    let pending = conn
        .prepare_cached("SELECT EXISTS (SELECT 1 FROM inbox_sources)")
        .and_then(|mut stmt| stmt.query_row([], |row| row.get::<_, bool>(0)));
    match pending {
        Ok(true) => {}
        Ok(false) => return,
        Err(err) => {
            tracing::error!(code = error_code(&err.into()), "inbox source probe failed");
            return;
        }
    }
    // Each row is attempted at most once per call: a failing row must not
    // burn its attempts inside one call.
    let mut cursor = 0_i64;
    for _ in 0..SOURCE_BATCHES {
        match drain_batch(conn, cursor) {
            Ok(Some(last)) => cursor = last,
            Ok(None) => break,
            Err(err) => {
                tracing::error!(code = error_code(&err), "inbox source drain failed");
                break;
            }
        }
    }
}

/// One transaction of the drain: rows with `id > after`. Returns the last
/// id read, or `None` when there was none.
fn drain_batch(conn: &mut Connection, after: i64) -> Result<Option<i64>, StoreError> {
    let tx = crate::store::immediate_tx(conn)?;
    let rows: Vec<(i64, String, String, String, i64)> = {
        let mut stmt = tx.prepare_cached(
            "SELECT id, kind, subject, entered_at, attempts FROM inbox_sources
              WHERE id > ?1 ORDER BY id LIMIT ?2",
        )?;
        stmt.query_map(params![after, SOURCE_BATCH], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect::<Result<_, _>>()?
    };
    let Some(last) = rows.last().map(|row| row.0) else {
        return Ok(None);
    };
    for (id, kind, subject, entered_at, attempts) in rows {
        tx.execute_batch("SAVEPOINT inbox_source")?;
        match apply_source(&tx, &kind, &subject, &entered_at) {
            Ok(()) => {
                tx.execute_batch("RELEASE inbox_source")?;
                tx.execute("DELETE FROM inbox_sources WHERE id = ?1", params![id])?;
            }
            Err(err) => {
                tx.execute_batch("ROLLBACK TO inbox_source; RELEASE inbox_source")?;
                let code = error_code(&err);
                if attempts + 1 >= MAX_SOURCE_ATTEMPTS {
                    tx.execute("DELETE FROM inbox_sources WHERE id = ?1", params![id])?;
                    tracing::error!(source = kind.as_str(), code, "inbox source dropped");
                } else {
                    tx.execute(
                        "UPDATE inbox_sources SET attempts = attempts + 1 WHERE id = ?1",
                        params![id],
                    )?;
                    tracing::warn!(source = kind.as_str(), code, "inbox source retried");
                }
            }
        }
    }
    tx.commit()?;
    Ok(Some(last))
}

fn apply_source(
    conn: &Connection,
    kind: &str,
    subject: &str,
    queued_at: &str,
) -> Result<(), StoreError> {
    match kind {
        "exited" => exit_fact(conn, subject, queued_at),
        "gate" => gate_fact(conn, subject, queued_at),
        // A kind a later build queued; nothing here can turn it into a fact.
        _ => Ok(()),
    }
}

/// The subject's process ended (OA6 evidence). Entered at the evidence's
/// journal time when the projection stamped one, else when the Hub first
/// held the end.
fn exit_fact(conn: &Connection, instance_id: &str, seen_at: &str) -> Result<(), StoreError> {
    let row: Option<(String, Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT lifecycle, ended_at, last_error FROM instances WHERE id = ?1",
            params![instance_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((lifecycle, ended_at, last_error)) = row else {
        return Ok(());
    };
    let entered_at = ended_at
        .as_deref()
        .and_then(canonical_at)
        .or_else(|| canonical_at(seen_at))
        .unwrap_or_else(now_rfc3339);
    let mut detail = json!({ "lifecycle": lifecycle });
    if let Some(error) = last_error.as_deref().map(bounded_text) {
        detail["lastError"] = json!(error);
    }
    let run = current_run(conn, instance_id)?;
    let fact = instance_fact(
        conn,
        InboxFactKind::Exited,
        instance_id,
        run,
        entered_at,
        detail,
    )?;
    record_about(conn, instance_id, fact)?;
    Ok(())
}

/// A gate job reached a terminal state: a fact for the lineage that
/// enqueued it. A job enqueued by a Human device has no initiator and gets
/// none.
fn gate_fact(conn: &Connection, job_id: &str, finished_at: &str) -> Result<(), StoreError> {
    let row: Option<(String, String, String)> = conn
        .query_row(
            "SELECT project_id, state, doc_json FROM gate_jobs WHERE id = ?1",
            params![job_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((project_id, state, doc)) = row else {
        return Ok(());
    };
    if !GATE_TERMINAL.contains(&state.as_str()) {
        return Ok(());
    }
    let doc: Value = serde_json::from_str(&doc).unwrap_or(Value::Null);
    let Some(subscriber) = doc
        .pointer("/initiator/lineageId")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return Ok(());
    };
    let initiator = doc
        .pointer("/initiator/instanceId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let entered_at = canonical_at(finished_at).unwrap_or_else(now_rfc3339);
    let mut detail = json!({ "jobId": job_id, "projectId": project_id, "state": state });
    for field in ["branch", "mode"] {
        if let Some(value) = doc.get(field).filter(|value| value.is_string()) {
            detail[field] = value.clone();
        }
    }
    record(
        conn,
        &subscriber,
        Fact {
            kind: InboxFactKind::Gate,
            subject_instance_id: initiator,
            key_subject: job_id.to_owned(),
            cap_subject: job_id.to_owned(),
            run: 0,
            entered_at,
            project_id: Some(project_id),
            payload: detail,
        },
    )?;
    Ok(())
}

/// A Node link came up (`online`) or a live one dropped. Facts follow the
/// last state the inbox announced, never the `hosts` row:
///
/// * the Hub marks every host offline at boot as bookkeeping (§8.3), which is
///   no observation and touches nothing here;
/// * a drop only stamps `lost_at`; the host-offline fact waits for the
///   host-lost grace (`tick`), so a reconnect inside it (a blip, or the
///   Hub's own restart dropping every link) announces nothing;
/// * a hello after an announced offline is the host-online fact; a host's
///   first hello announces nothing.
pub(crate) fn host_link(conn: &Connection, host_id: &str, online: bool) -> Result<(), StoreError> {
    let announced: Option<bool> = conn
        .query_row(
            "SELECT online FROM inbox_host_links WHERE host_id = ?1",
            params![host_id],
            |row| row.get(0),
        )
        .optional()?;
    let now = now_rfc3339();
    if !online {
        conn.execute(
            "INSERT INTO inbox_host_links (host_id, online, changed_at, lost_at)
             VALUES (?1, 1, ?2, ?2)
             ON CONFLICT(host_id) DO UPDATE SET lost_at = excluded.lost_at",
            params![host_id, now],
        )?;
        return Ok(());
    }
    conn.execute(
        "INSERT INTO inbox_host_links (host_id, online, changed_at, lost_at)
         VALUES (?1, 1, ?2, NULL)
         ON CONFLICT(host_id) DO UPDATE SET online = 1, lost_at = NULL,
            changed_at = CASE WHEN inbox_host_links.online = 1
                              THEN inbox_host_links.changed_at ELSE excluded.changed_at END",
        params![host_id, now],
    )?;
    if announced == Some(false) {
        host_facts(conn, host_id, InboxFactKind::HostOnline, &now)?;
    }
    Ok(())
}

/// Links that stayed down past the grace since they dropped (and since this
/// Hub started): the host-offline fact, once per loss.
fn due_host_offline(
    conn: &Connection,
    now: OffsetDateTime,
    clock: &TickClock,
) -> Result<usize, StoreError> {
    let cutoff = crate::config::rfc3339(now - clock.host_grace);
    if clock.booted_at > now - clock.host_grace {
        // Every link this process has not seen yet had the grace to return.
        return Ok(0);
    }
    let due: Vec<(String, String)> = {
        let mut stmt = conn.prepare_cached(
            "SELECT l.host_id, l.lost_at FROM inbox_host_links l
               JOIN hosts h ON h.id = l.host_id
              WHERE l.online = 1 AND l.lost_at IS NOT NULL AND l.lost_at <= ?1
                AND h.state <> 'online'",
        )?;
        stmt.query_map(params![cutoff], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?
    };
    let mut written = 0;
    for (host_id, lost_at) in due {
        conn.execute(
            "UPDATE inbox_host_links SET online = 0, changed_at = ?2 WHERE host_id = ?1",
            params![host_id, lost_at],
        )?;
        written += host_facts(conn, &host_id, InboxFactKind::HostOffline, &lost_at)?;
    }
    Ok(written)
}

/// One host fact for every lineage with a live current chapter whose scope
/// covers the host: continuity lineages, and opted-in lineages without
/// continuity.
fn host_facts(
    conn: &Connection,
    host_id: &str,
    kind: InboxFactKind,
    at: &str,
) -> Result<usize, StoreError> {
    let candidates: Vec<(String, Option<String>)> = {
        let mut stmt = conn.prepare_cached(
            "SELECT l.lineage_id, i.scope_json
               FROM lineages l JOIN instances i ON i.id = l.current_instance_id
              WHERE l.state <> 'paused' AND i.fenced_at IS NULL AND i.ended_at IS NULL
                AND i.lifecycle NOT IN ('exited', 'failed', 'closed')
             UNION
             SELECT s.lineage_id, i.scope_json
               FROM inbox_subscriptions s JOIN instances i ON i.id = s.lineage_id
              WHERE s.opt_in = 1
                AND NOT EXISTS (SELECT 1 FROM lineages l WHERE l.lineage_id = s.lineage_id)
                AND i.fenced_at IS NULL AND i.ended_at IS NULL
                AND i.lifecycle NOT IN ('exited', 'failed', 'closed')",
        )?;
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?
    };
    let online = kind == InboxFactKind::HostOnline;
    let mut written = 0;
    for (lineage_id, scope) in candidates {
        let scope = scope
            .and_then(|raw| serde_json::from_str::<remuda_protocol::InstanceScope>(&raw).ok())
            .unwrap_or_default();
        if !scope.allows_host(host_id) {
            continue;
        }
        let outcome = record(
            conn,
            &lineage_id,
            Fact {
                kind,
                subject_instance_id: None,
                key_subject: host_id.to_owned(),
                cap_subject: host_id.to_owned(),
                run: 0,
                entered_at: canonical_at(at).unwrap_or_else(now_rfc3339),
                project_id: scope.single_project_id().map(|id| id.as_id().to_string()),
                payload: json!({ "hostId": host_id, "online": online }),
            },
        )?;
        if matches!(outcome, Outcome::Recorded(_)) {
            written += 1;
        }
    }
    Ok(written)
}

/// F superseded a pending interaction of `predecessor` (D-057 §7.2 step 8):
/// a fact for the predecessor's subscriber, never for its own lineage.
pub(crate) fn interaction_superseded(
    conn: &Connection,
    predecessor: &str,
    interaction_id: &str,
    interaction_kind: &str,
    superseded_by: &str,
    at: &str,
) -> Result<(), StoreError> {
    let run = current_run(conn, predecessor)?;
    let mut fact = instance_fact(
        conn,
        InboxFactKind::InteractionSuperseded,
        predecessor,
        run,
        canonical_at(at).unwrap_or_else(now_rfc3339),
        json!({
            "interactionId": interaction_id,
            "interactionKind": interaction_kind,
            "supersededBy": superseded_by,
        }),
    )?;
    fact.key_subject = format!("{predecessor}/{interaction_id}");
    record_about(conn, predecessor, fact)?;
    Ok(())
}

/// A child was parked on a supply window: its model family on a provider
/// profile is cooling until `resets_at` (epoch seconds). The reset fact is
/// written when that time passes and the family is no longer cooling.
pub(crate) fn note_supply_park(
    conn: &Connection,
    instance_id: &str,
    profile_id: &str,
    family: &str,
    resets_at: i64,
) -> Result<(), StoreError> {
    let Some(resets) = OffsetDateTime::from_unix_timestamp(resets_at)
        .ok()
        .map(crate::config::rfc3339)
    else {
        return Ok(());
    };
    let run = current_run(conn, instance_id)?;
    conn.execute(
        "INSERT INTO inbox_supply_parks
            (instance_id, profile_id, family, run_generation, resets_at, parked_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(instance_id, profile_id, family) DO UPDATE SET
            run_generation = excluded.run_generation,
            resets_at = MAX(inbox_supply_parks.resets_at, excluded.resets_at),
            parked_at = excluded.parked_at",
        params![instance_id, profile_id, family, run, resets, now_rfc3339()],
    )?;
    Ok(())
}

/// Parks whose reset time has passed: a fact when the family is clear, a
/// later reset time when a newer window parked it again.
fn due_supply_resets(conn: &Connection, now: OffsetDateTime) -> Result<usize, StoreError> {
    let now_at = crate::config::rfc3339(now);
    let due: Vec<(String, String, String, i64, String)> = {
        let mut stmt = conn.prepare_cached(
            "SELECT instance_id, profile_id, family, run_generation, resets_at
               FROM inbox_supply_parks WHERE resets_at <= ?1",
        )?;
        stmt.query_map(params![now_at], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect::<Result<_, _>>()?
    };
    let mut written = 0;
    for (instance_id, profile_id, family, run, resets_at) in due {
        let supply: Option<String> = conn
            .query_row(
                "SELECT supply_json FROM provider_profiles WHERE id = ?1",
                params![profile_id],
                |row| row.get(0),
            )
            .optional()?;
        let windows = supply
            .and_then(|raw| serde_json::from_str::<remuda_protocol::SupplyProfile>(&raw).ok())
            .map(|supply| supply.windows)
            .unwrap_or_default();
        let still_cooling = windows
            .iter()
            .filter(|window| window.applies_to_family(&family))
            .filter_map(|window| window.cooldown_until)
            .filter(|until| *until > now.unix_timestamp())
            .max();
        if let Some(until) =
            still_cooling.and_then(|until| OffsetDateTime::from_unix_timestamp(until).ok())
        {
            conn.execute(
                "UPDATE inbox_supply_parks SET resets_at = ?4
                  WHERE instance_id = ?1 AND profile_id = ?2 AND family = ?3",
                params![
                    instance_id,
                    profile_id,
                    family,
                    crate::config::rfc3339(until)
                ],
            )?;
            continue;
        }
        conn.execute(
            "DELETE FROM inbox_supply_parks
              WHERE instance_id = ?1 AND profile_id = ?2 AND family = ?3",
            params![instance_id, profile_id, family],
        )?;
        let fact = instance_fact(
            conn,
            InboxFactKind::SupplyReset,
            &instance_id,
            run,
            resets_at.clone(),
            json!({ "profileId": profile_id, "family": family, "resetsAt": resets_at }),
        )?;
        if matches!(
            record_about(conn, &instance_id, fact)?,
            Outcome::Recorded(_)
        ) {
            written += 1;
        }
    }
    Ok(written)
}

/// What the time-driven facts measure against besides `now`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TickClock {
    /// When this Hub process opened its store.
    pub(crate) booted_at: OffsetDateTime,
    /// How long a link stays down before its host is announced offline: the
    /// host-lost grace (`HubConfig::host_lost_grace_ms`).
    pub(crate) host_grace: time::Duration,
}

/// Facts whose state is entered by time passing rather than by a write:
/// supply resets and hosts that stayed offline. One writer job; every fact
/// it writes is claimed once by its key.
pub(crate) fn tick(
    conn: &mut Connection,
    now: OffsetDateTime,
    clock: TickClock,
) -> Result<usize, StoreError> {
    let tx = crate::store::immediate_tx(conn)?;
    let written = due_supply_resets(&tx, now)? + due_host_offline(&tx, now, &clock)?;
    tx.commit()?;
    Ok(written)
}

/// Hooks for the wake tasks (`cb-wake-engine`, context-percentage wake-ups).
/// They record through [`record`] with these kinds; nothing here decides
/// when they fire.
pub(crate) mod hooks {
    /// Kind a due wake-up records (`cb-wake-engine`).
    #[allow(dead_code)]
    pub(crate) const WAKE_DUE: remuda_protocol::InboxFactKind =
        remuda_protocol::InboxFactKind::WakeDue;
    /// Kind a context-percentage crossing records (the wake tasks).
    #[allow(dead_code)]
    pub(crate) const CONTEXT_THRESHOLD: remuda_protocol::InboxFactKind =
        remuda_protocol::InboxFactKind::ContextThreshold;
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

/// Scheduler ticks between two inbox evaluations (the 1 s schedule tick).
pub(crate) const TICK_EVERY: u64 = 15;

/// One inbox evaluation from the scheduler loop. A failure is logged by
/// error code and retried on the next evaluation: every due row stays due
/// until its fact commits.
pub(crate) async fn run_tick(state: &AppState) {
    let clock = TickClock {
        booted_at: state.store.opened_at(),
        host_grace: host_grace(&state.config),
    };
    if let Err(err) = state
        .store
        .inbox_tick(OffsetDateTime::now_utc(), clock)
        .await
    {
        tracing::error!(code = error_code(&err), "inbox tick failed");
    }
}

/// The configured host-lost grace as a duration.
pub(crate) fn host_grace(config: &crate::config::HubConfig) -> time::Duration {
    time::Duration::milliseconds(i64::try_from(config.host_lost_grace_ms).unwrap_or(i64::MAX))
}

impl Store {
    /// One evaluation of the time-driven facts at `now`.
    pub(crate) async fn inbox_tick(
        &self,
        now: OffsetDateTime,
        clock: TickClock,
    ) -> Result<usize, StoreError> {
        self.run_named("inbox_tick", move |conn| tick(conn, now, clock))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path()).expect("store");
        (dir, store)
    }

    /// A bare instance row: `lineage` and `parent` as given, `spec` merged.
    fn instance(conn: &Connection, id: &str, lineage: &str, parent: Option<&str>, origin: &str) {
        let spec = json!({ "origin": origin, "parentInstanceId": parent });
        conn.execute(
            "INSERT INTO instances
                (id, host_id, kind, driver, lifecycle, activity, connectivity, journal_id,
                 spec_json, created_at, updated_at, lineage_id, generation)
             VALUES (?1, 'hst_t', 'claude', 'claude-pty', 'running', 'working', 'connected',
                     ?1, ?2, ?3, ?3, ?4, 1)",
            params![id, spec.to_string(), now_rfc3339(), lineage],
        )
        .expect("instance");
    }

    fn continuity(conn: &Connection, lineage: &str, current: &str) {
        conn.execute(
            "INSERT INTO lineages (lineage_id, current_instance_id, generation, state, updated_at)
             VALUES (?1, ?2, 1, 'running', ?3)",
            params![lineage, current, now_rfc3339()],
        )
        .expect("lineage");
    }

    fn facts(conn: &Connection, subscriber: &str) -> Vec<(String, String)> {
        let mut stmt = conn
            .prepare(
                "SELECT kind, dedupe_key FROM inbox_facts
                  WHERE subscriber_lineage_id = ?1 ORDER BY id",
            )
            .unwrap();
        stmt.query_map(params![subscriber], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// §10.1 item 4: a parent edge that leads back into the subject's own
    /// lineage names no subscriber, so nothing about the chapter reaches it.
    #[tokio::test]
    async fn a_parent_edge_into_the_own_lineage_names_no_subscriber() {
        let (_dir, store) = store().await;
        let (own, other) = store
            .run_named("t", |conn| {
                instance(conn, "ins_a", "ins_a", None, "human");
                continuity(conn, "ins_a", "ins_b");
                // Chapter 2 of lineage ins_a whose edge points at chapter 1.
                instance(conn, "ins_b", "ins_a", Some("ins_a"), "human");
                // A child in a lineage of its own.
                instance(conn, "ins_c", "ins_c", Some("ins_b"), "agent");
                let own = subscriber_of(conn, "ins_b")?;
                let other = subscriber_of(conn, "ins_c")?;
                let fact = instance_fact(
                    conn,
                    InboxFactKind::TurnDone,
                    "ins_b",
                    1,
                    "2026-11-01T00:00:00.000Z".into(),
                    json!({}),
                )?;
                assert_eq!(record_about(conn, "ins_b", fact)?, Outcome::NotReceived);
                assert!(facts(conn, "ins_a").is_empty());
                Ok((own, other))
            })
            .await
            .unwrap();
        assert_eq!(own, None);
        assert_eq!(other.as_deref(), Some("ins_a"));
    }

    /// A gate job's terminal state, written by any of the queue's paths,
    /// queues a source in the same write; the drain after the job turns it
    /// into one fact for the enqueuing lineage. A Human's job has none.
    #[tokio::test]
    async fn a_terminal_gate_job_is_one_fact_for_its_initiator_lineage() {
        let (_dir, store) = store().await;
        store
            .run_named("t", |conn| {
                instance(conn, "ins_seat", "ins_seat", None, "human");
                continuity(conn, "ins_seat", "ins_seat");
                for (id, initiator) in [
                    (
                        "gjb_agent",
                        json!({"instanceId": "ins_seat", "lineageId": "ins_seat", "generation": 1}),
                    ),
                    ("gjb_human", Value::Null),
                ] {
                    let mut doc = json!({ "id": id, "branch": "wt/x/b-md", "mode": "verify" });
                    if !initiator.is_null() {
                        doc["initiator"] = initiator;
                    }
                    conn.execute(
                        "INSERT INTO gate_jobs (id, project_id, state, queued_at, doc_json,
                                                created_at, updated_at)
                         VALUES (?1, 'prj_t', 'running', ?2, ?3, ?2, ?2)",
                        params![id, "2026-11-01T00:00:00.000Z", doc.to_string()],
                    )?;
                }
                Ok(())
            })
            .await
            .unwrap();
        for (id, state) in [
            ("gjb_agent", "canceling"),
            ("gjb_agent", "passed"),
            ("gjb_human", "failed"),
        ] {
            let (id, state) = (id.to_owned(), state.to_owned());
            store
                .run_named("t", move |conn| {
                    conn.execute(
                        "UPDATE gate_jobs SET state = ?2, updated_at = ?3 WHERE id = ?1",
                        params![id, state, "2026-11-01T00:05:00.000Z"],
                    )?;
                    Ok(())
                })
                .await
                .unwrap();
        }
        // The drain ran after each job: nothing is left queued.
        let (left, facts) = store
            .run_named("t", |conn| {
                let left: i64 =
                    conn.query_row("SELECT COUNT(*) FROM inbox_sources", [], |row| row.get(0))?;
                Ok((left, facts(conn, "ins_seat")))
            })
            .await
            .unwrap();
        assert_eq!(left, 0);
        assert_eq!(
            facts,
            [(
                "gate".to_owned(),
                "gate:gjb_agent:0:2026-11-01T00:05:00.000Z".to_owned()
            )]
        );
    }

    /// A source row whose fact cannot be written stays for the next job and
    /// is dropped after `MAX_SOURCE_ATTEMPTS`, never retried forever.
    #[tokio::test]
    async fn a_poisoned_source_is_retried_then_dropped() {
        let (_dir, store) = store().await;
        store
            .run_named("t", |conn| {
                // A gate source whose job table cannot be read: every attempt
                // at its fact fails.
                conn.execute_batch(
                    "INSERT INTO inbox_sources (kind, subject, entered_at)
                     VALUES ('gate', 'gjb_missing_table', '2026-11-01T00:00:00.000Z');
                     ALTER TABLE gate_jobs RENAME TO gate_jobs_away;",
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let mut seen = Vec::new();
        for _ in 0..MAX_SOURCE_ATTEMPTS + 1 {
            seen.push(
                store
                    .run_named("t", |conn| {
                        Ok(conn
                            .query_row(
                                "SELECT attempts FROM inbox_sources WHERE subject = 'gjb_missing_table'",
                                [],
                                |row| row.get::<_, i64>(0),
                            )
                            .optional()?)
                    })
                    .await
                    .unwrap(),
            );
        }
        // Each job's drain tried it once; after the last attempt it is gone.
        assert_eq!(seen.first().copied().flatten(), Some(1));
        assert_eq!(seen.last().copied().flatten(), None, "{seen:?}");
    }

    /// A child parked on a supply window yields its reset fact once the
    /// window has passed and the family is clear; a newer park moves it.
    #[tokio::test]
    async fn a_supply_park_resets_once_the_family_clears() {
        let (_dir, store) = store().await;
        let t = |s: &str| OffsetDateTime::parse(s, &Rfc3339).unwrap().unix_timestamp();
        let first = t("2026-11-01T01:00:00Z");
        let second = t("2026-11-01T02:00:00Z");
        let window = |until: i64| {
            json!({ "windows": [{ "id": "model", "appliesTo": ["opus"],
                                  "usedPercent": 100, "source": "observed",
                                  "cooldownUntil": until }] })
        };
        let supply = window(first).to_string();
        store
            .run_named("t", move |conn| {
                instance(conn, "ins_seat", "ins_seat", None, "human");
                continuity(conn, "ins_seat", "ins_seat");
                instance(conn, "ins_w", "ins_w", Some("ins_seat"), "agent");
                conn.execute(
                    "INSERT INTO provider_profiles (id, name, kind, supply_json, created_at, updated_at)
                     VALUES ('pvp_t', 'relay', 'gateway', ?1, ?2, ?2)",
                    params![supply, now_rfc3339()],
                )?;
                note_supply_park(conn, "ins_w", "pvp_t", "opus", first)
            })
            .await
            .unwrap();
        let clock = TickClock {
            booted_at: OffsetDateTime::UNIX_EPOCH,
            host_grace: time::Duration::minutes(10),
        };
        let at = |s: &str| OffsetDateTime::parse(s, &Rfc3339).unwrap();
        assert_eq!(
            store
                .inbox_tick(at("2026-11-01T00:30:00Z"), clock)
                .await
                .unwrap(),
            0
        );
        // Parked again before the first reset passed: the window now runs
        // to the second time.
        let supply = window(second).to_string();
        store
            .run_named("t", move |conn| {
                conn.execute(
                    "UPDATE provider_profiles SET supply_json = ?1 WHERE id = 'pvp_t'",
                    params![supply],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(
            store
                .inbox_tick(at("2026-11-01T01:30:00Z"), clock)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .inbox_tick(at("2026-11-01T02:00:30Z"), clock)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .inbox_tick(at("2026-11-01T03:00:00Z"), clock)
                .await
                .unwrap(),
            0
        );
        let facts = store
            .run_named("t", |conn| Ok(facts(conn, "ins_seat")))
            .await
            .unwrap();
        assert_eq!(
            facts,
            [(
                "supply-reset".to_owned(),
                "supply-reset:ins_w:0:2026-11-01T02:00:00.000Z".to_owned()
            )]
        );
    }

    #[test]
    fn times_are_canonical_whatever_offset_the_source_wrote() {
        assert_eq!(
            canonical_at("2026-11-01T09:00:00+01:00").as_deref(),
            Some("2026-11-01T08:00:00.000Z")
        );
        assert_eq!(
            canonical_at("2026-11-01T08:00:00.123456Z").as_deref(),
            Some("2026-11-01T08:00:00.123Z")
        );
        assert_eq!(canonical_at("yesterday"), None);
    }
}
