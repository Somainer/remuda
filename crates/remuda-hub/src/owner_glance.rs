//! home-glance-d060 (H10): `GET /v1/owner/glance`, the owner's world as one
//! read-only projection (home.md §5.3).
//!
//! D-060's `GET /v1/lineages/{id}/glance` covers one lineage. The home needs
//! the owner's whole world instead: **Main's lineage subtree plus the sessions
//! the owner opened by hand** (documented in home.md §5.3). This route is
//! Human-only — an Agent, a Bot and a narrowed Human (a Human credential that
//! names an instance and is re-stamped Agent by [`crate::agent_scope::caller`])
//! all get 403.
//!
//! Everything here is derived from rows the Hub already stores: the instance
//! index, its pending interactions, the task ledger, the owner's seen seq and a
//! bounded journal tail per working row. The Hub never calls a model, and every
//! string a session or a tool produced is untrusted data; the headline reuses
//! the worker digest's redaction and clips.
//!
//! Cost is one bounded query set per call: one recursive scope query, one
//! instance page, the pending interactions of that scope, the blocked tasks,
//! and at most [`OWNER_GLANCE_ROW_CAP`] journal tails bounded exactly as the
//! digest bounds them ([`crate::digest::JOURNAL_TAIL_EVENTS`]).

use crate::AppState;
use crate::agent_scope::{caller, origin};
use crate::auth::require_origin;
use crate::error::HubError;
use crate::http::map_store;
use crate::store::{InteractionRecord, Store};
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use remuda_protocol::{
    GlanceMore, InputOrigin, OWNER_GLANCE_ROW_CAP, OwnerGlance, OwnerGlanceCounts, OwnerGlanceRow,
};
use rusqlite::params;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use time::{OffsetDateTime, UtcOffset};

/// RFC3339 timestamps are fixed-width, so a text comparison is chronological.
const OWNER_SCOPE: &str = "owner";
/// A headline is one line; this bounds it.
const HEADLINE_CHARS: usize = 120;
/// The title a row carries; the digest clips a name to 64.
const TITLE_CHARS: usize = 64;

pub(crate) fn routes() -> Router<AppState> {
    Router::new().route("/v1/owner/glance", get(owner_glance))
}

// ── the Main seat (home.md §5.3 / §6.1) ────────────────────────────────────

/// One `address-owner` chapter, as the Main resolver reads it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MainSeatRow {
    pub instance_id: String,
    pub lineage_id: String,
    pub generation: i64,
    pub lifecycle: String,
    #[serde(default)]
    pub last_error: Option<String>,
    /// Whether authority moved away from this chapter (`fenced_at` set).
    pub fenced: bool,
    /// `updated_at` (else `created_at`) as Unix milliseconds.
    pub time_ms: i64,
}

/// Lifecycles that end a Main chapter (the client's `MAIN_TERMINAL`).
const MAIN_TERMINAL: &[&str] = &["exited", "failed", "closing"];

/// The chapter is the live Main: not fenced, and either not ended or a §5
/// contact-loss row (`host-contact-lost`, no end), which may still run.
fn main_seat_live(row: &MainSeatRow) -> bool {
    if row.fenced {
        return false;
    }
    !MAIN_TERMINAL.contains(&row.lifecycle.as_str())
        || row.last_error.as_deref() == Some(crate::store::HOST_LOST_MARKER)
}

/// The more recently touched row; equal times fall to the lower id, so the
/// pick never depends on the order the rows were read in.
fn main_seat_later<'a>(a: &'a MainSeatRow, b: &'a MainSeatRow) -> &'a MainSeatRow {
    if a.time_ms != b.time_ms {
        return if a.time_ms > b.time_ms { a } else { b };
    }
    if a.instance_id <= b.instance_id { a } else { b }
}

/// The newer chapter: a higher generation, then [`main_seat_later`].
fn main_seat_newer<'a>(a: &'a MainSeatRow, b: &'a MainSeatRow) -> &'a MainSeatRow {
    if a.generation != b.generation {
        return if a.generation > b.generation { a } else { b };
    }
    main_seat_later(a, b)
}

/// THE Main resolution, one definition for the Hub (home.md §5.3, §6.1),
/// written to match the client's `resolveMain` (`web/src/lib/store.ts`); the
/// shared cases in `tests/fixtures/main_seat_cases.json` hold both to it.
///
/// 1. The live `address-owner` chapter ([`main_seat_live`]), the newest when
///    a chapter races its successor.
/// 2. Otherwise — Main paused, closed or ended — the newest chapter of each
///    `address-owner` lineage, and of those the most recently touched one.
///
/// A paused Main therefore still resolves, so its subtree (the workers it
/// dispatched, still running) stays in the owner's world.
pub(crate) fn pick_main_seat(seats: &[MainSeatRow]) -> Option<&MainSeatRow> {
    if let Some(live) = seats
        .iter()
        .filter(|row| main_seat_live(row))
        .reduce(main_seat_newer)
    {
        return Some(live);
    }
    let mut newest: std::collections::BTreeMap<&str, &MainSeatRow> =
        std::collections::BTreeMap::new();
    for row in seats {
        newest
            .entry(row.lineage_id.as_str())
            .and_modify(|current| *current = main_seat_newer(current, row))
            .or_insert(row);
    }
    newest.into_values().reduce(main_seat_later)
}

fn rfc3339_ms(stamp: &str) -> Option<i64> {
    OffsetDateTime::parse(stamp, &time::format_description::well_known::Rfc3339)
        .ok()
        .and_then(|at| i64::try_from(at.unix_timestamp_nanos() / 1_000_000).ok())
}

/// Every `address-owner` chapter, then [`pick_main_seat`].
pub(crate) fn resolve_main_seat(
    conn: &rusqlite::Connection,
) -> Result<Option<MainSeatRow>, crate::store::StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, COALESCE(lineage_id, id), generation, lifecycle, last_error,
                fenced_at IS NOT NULL, updated_at, created_at
           FROM instances
          WHERE grants_json LIKE '%\"address-owner\"%'",
    )?;
    let seats = stmt
        .query_map([], |row| {
            let updated: Option<String> = row.get(6)?;
            let created: Option<String> = row.get(7)?;
            Ok(MainSeatRow {
                instance_id: row.get(0)?,
                lineage_id: row.get(1)?,
                generation: row.get::<_, Option<i64>>(2)?.unwrap_or(1),
                lifecycle: row.get(3)?,
                last_error: row.get(4)?,
                fenced: row.get(5)?,
                time_ms: updated
                    .as_deref()
                    .and_then(rfc3339_ms)
                    .or_else(|| created.as_deref().and_then(rfc3339_ms))
                    .unwrap_or(0),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(pick_main_seat(&seats).cloned())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GlanceQuery {
    /// The owner's local-day offset in minutes east of UTC. Absent reads as
    /// UTC; the range is clamped to the real-world ±14 h.
    #[serde(default)]
    tz_offset_minutes: Option<i32>,
}

/// 403 for anything but the owner's own Human device (§5.3 authority).
fn owner_only() -> HubError {
    HubError::Forbidden.with_next(
        "owner-only",
        crate::error::NextStep::new(
            crate::error::action::ASK_OWNER,
            "the owner glance is read only from the owner's own device; do not retry with \
             this credential",
        ),
    )
}

async fn owner_glance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<GlanceQuery>,
) -> Result<Json<OwnerGlance>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = caller(&state, &headers).await?;
    if origin(&device) != InputOrigin::Human {
        tracing::info!(category = "owner-glance", outcome = "refused-not-human");
        return Err(owner_only());
    }
    let offset_minutes = query
        .tz_offset_minutes
        .unwrap_or(0)
        .clamp(-14 * 60, 14 * 60);
    Ok(Json(
        collect_owner_glance(&state, &device, offset_minutes).await?,
    ))
}

/// The owner's local-day start (00:00 local), expressed in UTC so the
/// UTC-written `turn_end_at` stamp compares textually.
fn local_day_start(now: OffsetDateTime, offset_minutes: i32) -> OffsetDateTime {
    // The route clamps to ±14 h, so this is always a real offset; a value that
    // somehow is not falls back to UTC rather than failing the glance.
    let offset =
        UtcOffset::from_whole_seconds(offset_minutes.saturating_mul(60)).unwrap_or(UtcOffset::UTC);
    now.to_offset(offset)
        .replace_time(time::Time::MIDNIGHT)
        .to_offset(UtcOffset::UTC)
}

/// One item of the merged `/v1/interactions` page as a pending interaction of
/// the glance. The page merges the durable rows, the Hub's in-memory D-017
/// grants and each Node's live copy; the owner's 需要你 count and the queue are
/// derived from that same page, so a live card the owner can answer is never
/// missing here.
fn merged_item_pending(item: &Value, now_unix: i64) -> Option<PendingInteraction> {
    let instance_id = item.get("instanceId").and_then(Value::as_str)?.to_string();
    // Only a pending row is in the 待你处理 queue (deriveInboxQueue).
    if item.get("state").and_then(Value::as_str) != Some("pending") {
        return None;
    }
    let interaction_id = item
        .get("interactionId")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)?
        .to_string();
    let interaction = item
        .get("interaction")
        .cloned()
        .unwrap_or_else(|| item.clone());
    let record = InteractionRecord {
        interaction_id,
        instance_id,
        host_id: item
            .get("hostId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        kind: item
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("approval")
            .to_string(),
        state: "pending".to_string(),
        blocking: item
            .get("blocking")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        // `pending_parts` reads `/payload/interaction` or `/interaction`, and
        // the merged item carries the request inside that entity.
        payload: serde_json::json!({ "interaction": interaction }),
        created_at: item
            .get("createdAt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        updated_at: item
            .get("updatedAt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    };
    let expired = interaction_deadline_passed(&record, now_unix);
    Some(PendingInteraction { record, expired })
}

/// Render the owner glance at `offset_minutes` east of UTC.
pub(crate) async fn collect_owner_glance(
    state: &AppState,
    device: &crate::store::Device,
    offset_minutes: i32,
) -> Result<OwnerGlance, HubError> {
    let now = OffsetDateTime::now_utc();
    // The owner's local day start, expressed in UTC so the turn-end stamp
    // (written in UTC) compares textually.
    let day_start = local_day_start(now, offset_minutes);
    let day_start = crate::config::rfc3339(day_start);
    let now_str = crate::config::rfc3339(now);
    let snapshot = state
        .store
        .owner_glance_snapshot(day_start, now_str)
        .await
        .map_err(map_store)?;
    // The queue's own source: the merged `/v1/interactions` page (durable rows
    // + the Hub's in-memory grants + each Node's live copy), UNFILTERED — §5.1
    // says the ① count, the ④ header and the OS badge are one definition, and
    // those two are derived over every interaction the owner can see. Only the
    // glance's ROWS are narrowed to the owner scope below.
    let merged = crate::interactions::merged_interactions(state, device, None, None, None).await?;
    let pending: Vec<PendingInteraction> = merged
        .iter()
        .filter_map(|item| merged_item_pending(item, now.unix_timestamp()))
        .collect();
    Ok(render_owner_glance(
        snapshot,
        pending,
        now.unix_timestamp(),
        offset_minutes,
    ))
}

// ── the snapshot ────────────────────────────────────────────────────────────

/// One instance of the owner scope, as the counts and rows read it.
pub(crate) struct ScopeInstance {
    pub instance_id: String,
    pub title: Option<String>,
    pub lifecycle: String,
    pub activity: String,
    pub updated_at: String,
    pub task_id: Option<String>,
    pub has_turn_error: bool,
    pub in_main_lineage: bool,
    pub unseen: bool,
    pub done_today: bool,
}

/// One pending interaction the owner can answer.
pub(crate) struct PendingInteraction {
    pub record: InteractionRecord,
    /// The deadline passed: the inbox projects it `expired`, so §5.1 does not
    /// count it and neither does this projection.
    pub expired: bool,
}

/// One owner-blocked task (unarchived, with a blocked reason).
pub(crate) struct BlockedTask {
    pub task_id: String,
}

/// Everything the route reads, in one consistent snapshot.
pub(crate) struct OwnerGlanceSnapshot {
    /// The owner-scope instance ids (the same list the rows are filtered by).
    pub scope: Vec<String>,
    pub instances: Vec<ScopeInstance>,
    pub blocked_tasks: Vec<BlockedTask>,
    /// EVERY instance carrying a task (`instance_id`, `task_id`), fleet-wide:
    /// §5.1 B asks whether any session of a blocked task is already in the
    /// queue, and that queue is not owner-scoped.
    pub task_sessions: Vec<(String, String)>,
    /// Bounded journal tail per session that ranks working (id → events).
    pub journals: HashMap<String, Vec<Value>>,
    /// Session the resolved Main chapter is, when the Hub has a Main.
    pub main_instance: Option<String>,
}

impl Store {
    /// The owner-scope rows the glance needs, read in one transaction.
    pub(crate) async fn owner_glance_snapshot(
        &self,
        day_start: String,
        now: String,
    ) -> Result<OwnerGlanceSnapshot, crate::store::StoreError> {
        self.read("owner_glance_snapshot", move |conn| {
            let tx = conn.unchecked_transaction()?;
            // The Main seat, resolved exactly as the client resolves it
            // ([`resolve_main_seat`]): a paused or closed Main still names its
            // lineage, so the workers it dispatched stay in scope.
            let main: Option<(String, String)> =
                resolve_main_seat(&tx)?.map(|seat| (seat.lineage_id, seat.instance_id));
            let main_lineage = main.as_ref().map(|(lineage, _)| lineage.clone());
            // §5.3 scope: Main's lineage subtree, plus the sessions the owner
            // opened by hand. ONE rule: a session is the owner's when the ROOT
            // of its resume/continuation chain — its generation-1 chapter — is
            // a TRUE root (no parent, no `resumedFrom`, its own lineage) whose
            // Hub-stamped creator was a Human device (or the upgrade
            // backfill's `legacy`). The chain then follows every
            // `resumedFrom` link and every continuation chapter of that
            // lineage (2nd, 3rd, …), so a Bot root relaunched by a human, a
            // supervisor-respawned worker and an auto-continued successor all
            // stay out: their ROOT is not hand-opened. A hand-opened session
            // never drags in the children it dispatched — those carry it as
            // `parentInstanceId`, not as a resume/continuation link.
            let mut scope_stmt = tx.prepare(
                "WITH RECURSIVE
                     main_scope(id) AS (
                         SELECT id FROM instances WHERE COALESCE(lineage_id, id) = ?1
                         UNION
                         SELECT i.id FROM instances i
                           JOIN main_scope s
                             ON json_extract(i.spec_json, '$.parentInstanceId') = s.id
                     ),
                     hand(id) AS (
                         -- A TRUE root the owner's device created (or the
                         -- upgrade backfill could only call `legacy`).
                         SELECT id FROM instances
                          WHERE creator_origin IN ('human', 'legacy')
                            AND json_extract(spec_json, '$.parentInstanceId') IS NULL
                            AND json_extract(spec_json, '$.resumedFrom') IS NULL
                            AND (lineage_id IS NULL OR lineage_id = id)
                         UNION
                         -- A resume of a hand session, or ANY continuation
                         -- chapter of a hand lineage (generation 2, 3, …).
                         SELECT i.id FROM instances i
                           JOIN hand h
                             ON json_extract(i.spec_json, '$.resumedFrom') = h.id
                             OR (i.lineage_id = h.id AND i.generation > 1)
                     )
                 SELECT id FROM main_scope
                 UNION
                 SELECT id FROM hand",
            )?;
            let scope: Vec<String> = scope_stmt
                .query_map(params![main_lineage], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            drop(scope_stmt);
            let scope_json = serde_json::to_string(&scope)
                .map_err(|err| crate::store::StoreError::Id(err.to_string()))?;
            // Every scope instance the rank/count rules read. `turn_end_at` is
            // the local-day "done today" stamp (see `seen::fold_turn_end`).
            let mut rows_stmt = tx.prepare(
                "SELECT i.id, i.title, i.lifecycle, i.activity, i.updated_at,
                        i.task_id, i.turn_error_json, i.lineage_id, i.durable_seq,
                        (SELECT v.seen_seq FROM instance_views v
                          WHERE v.instance_id = i.id AND v.scope = ?2),
                        i.turn_end_seq, i.turn_end_at
                   FROM instances i
                   JOIN json_each(?1) s ON s.value = i.id",
            )?;
            let instances = rows_stmt
                .query_map(params![scope_json, OWNER_SCOPE], |row| {
                    let id: String = row.get(0)?;
                    let title: Option<String> = row.get(1)?;
                    let lifecycle: String = row.get(2)?;
                    let activity: String = row.get(3)?;
                    let updated_at: String = row.get(4)?;
                    let task_id: Option<String> = row.get(5)?;
                    let turn_error: Option<String> = row.get(6)?;
                    let lineage: Option<String> = row.get(7)?;
                    let _durable: i64 = row.get(8)?;
                    let seen_seq: Option<i64> = row.get(9)?;
                    let turn_end_seq: Option<i64> = row.get(10)?;
                    let turn_end_at: Option<String> = row.get(11)?;
                    Ok(ScopeInstance {
                        instance_id: id,
                        title,
                        lifecycle,
                        activity,
                        updated_at,
                        task_id,
                        has_turn_error: turn_error
                            .as_deref()
                            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                            .and_then(|value| {
                                value
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .map(str::trim)
                                    .map(|text| !text.is_empty())
                            })
                            .unwrap_or(false),
                        in_main_lineage: lineage
                            .as_deref()
                            .is_some_and(|value| Some(value) == main_lineage.as_deref()),
                        unseen: turn_end_seq.is_some_and(|end| end > seen_seq.unwrap_or(-1)),
                        done_today: turn_end_seq.is_some()
                            && turn_end_at.is_some_and(|at| at >= day_start && at <= now),
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows_stmt.finalize().ok();
            // Only the DISPLAYED working page's journal tail is read (bounded
            // exactly as the digest bounds it). The page is ordered by the
            // same key the renderer paints with (updatedAt desc, id asc) and
            // then cut, so every painted row has its headline.
            let mut working_ids: Vec<&ScopeInstance> = instances
                .iter()
                .filter(|row| rank_working(&row.lifecycle, &row.activity))
                .collect();
            working_ids.sort_by(|a, b| {
                (b.updated_at.as_str(), a.instance_id.as_str())
                    .cmp(&(a.updated_at.as_str(), b.instance_id.as_str()))
            });
            working_ids.truncate(OWNER_GLANCE_ROW_CAP);
            let mut blocked_tasks = Vec::new();
            let mut task_stmt = tx.prepare(
                "SELECT id FROM tasks
                  WHERE json_extract(doc_json, '$.archivedAt') IS NULL
                    AND COALESCE(json_extract(doc_json, '$.blockedReason'), '') <> ''",
            )?;
            blocked_tasks.extend(
                task_stmt
                    .query_map([], |row| {
                        Ok(BlockedTask {
                            task_id: row.get(0)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?,
            );
            task_stmt.finalize().ok();
            // §5.1 B's dedupe is fleet-wide: the queue it compares against is
            // not owner-scoped.
            let mut task_sessions = Vec::new();
            let mut session_stmt =
                tx.prepare("SELECT id, task_id FROM instances WHERE task_id IS NOT NULL")?;
            task_sessions.extend(
                session_stmt
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?,
            );
            session_stmt.finalize().ok();
            // The working rows' headlines: one bounded journal tail each, the
            // same 256-event window the worker digest reads.
            let mut journals = HashMap::new();
            for row in &working_ids {
                journals.insert(
                    row.instance_id.clone(),
                    journal_tail(&tx, &row.instance_id, crate::digest::JOURNAL_TAIL_EVENTS)?,
                );
            }
            tx.commit()?;
            Ok(OwnerGlanceSnapshot {
                scope,
                instances,
                blocked_tasks,
                task_sessions,
                journals,
                main_instance: main.map(|(_, instance)| instance),
            })
        })
        .await
    }
}

fn interaction_deadline_passed(record: &InteractionRecord, now: i64) -> bool {
    let deadline = record
        .payload
        .pointer("/payload/interaction/deadline")
        .or_else(|| record.payload.pointer("/interaction/deadline"));
    let Some(deadline) = deadline else {
        return false;
    };
    if deadline.get("state").and_then(Value::as_str) != Some("known") {
        return false;
    }
    deadline
        .get("value")
        .and_then(Value::as_str)
        .and_then(|value| {
            OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).ok()
        })
        .is_some_and(|value| value.unix_timestamp() < now)
}

fn journal_tail(
    conn: &rusqlite::Connection,
    instance_id: &str,
    cap: i64,
) -> Result<Vec<Value>, crate::store::StoreError> {
    // The newest `cap` rows, returned ascending so `scan_tail` sees them in
    // journal order.
    let mut stmt = conn.prepare_cached(
        "SELECT payload_json FROM (
             SELECT payload_json, seq FROM journal
              WHERE instance_id = ?1
              ORDER BY seq DESC LIMIT ?2
         ) ORDER BY seq",
    )?;
    let rows = stmt
        .query_map(params![instance_id, cap], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|raw| serde_json::from_str(&raw).ok())
        .collect())
}

// ── rank (home.md §3.1) ─────────────────────────────────────────────────────

fn rank_is_live(lifecycle: &str) -> bool {
    !matches!(
        lifecycle,
        "requested"
            | "preparing"
            | "starting"
            | "unknown"
            | "reconciling"
            | "exited"
            | "failed"
            | "closing"
    )
}

fn activity_is_waiting(activity: &str) -> bool {
    // The store writes `blocked`; the wire projection calls it
    // waiting-interaction. Both are rank 1.
    matches!(activity, "blocked" | "waiting-interaction")
}

/// §3.1 rank 3: a live turn, or a live session whose activity the Hub has not
/// classified (that row is treated as working, never idle).
fn rank_working(lifecycle: &str, activity: &str) -> bool {
    if !rank_is_live(lifecycle) || activity_is_waiting(activity) {
        return false;
    }
    matches!(activity, "working" | "draining")
        || !matches!(activity, "idle" | "working" | "draining")
}

/// §3.1 rank 2: idle, the owner has not opened the last settled turn, and the
/// turn did not fail.
fn rank_done_unseen(instance: &ScopeInstance) -> bool {
    rank_is_live(&instance.lifecycle)
        && instance.activity == "idle"
        && instance.unseen
        && !instance.has_turn_error
}

// ── rendering ───────────────────────────────────────────────────────────────

fn clip(text: &str, cap: usize) -> String {
    let text: String = text.chars().take(cap).collect();
    text
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A redacted, single-line, clipped sentence.
fn headline_text(text: &str) -> String {
    clip(
        &one_line(&crate::digest::redact_secrets(text)),
        HEADLINE_CHARS,
    )
}

fn render_owner_glance(
    snapshot: OwnerGlanceSnapshot,
    pending: Vec<PendingInteraction>,
    now: i64,
    offset_minutes: i32,
) -> OwnerGlance {
    // §5.1 A counts INTERACTIONS (the inbox queue is one row per interaction),
    // over EVERY interaction the owner can answer — the same unscoped set the
    // ④ header and the OS badge derive from. Only the ROWS below are narrowed
    // to the owner scope.
    let live_pending: Vec<&PendingInteraction> =
        pending.iter().filter(|pending| !pending.expired).collect();
    let queued: std::collections::HashSet<&str> = live_pending
        .iter()
        .map(|pending| pending.record.instance_id.as_str())
        .collect();
    let mut sessions_by_task: HashMap<&str, Vec<&str>> = HashMap::new();
    for (instance_id, task_id) in &snapshot.task_sessions {
        sessions_by_task
            .entry(task_id.as_str())
            .or_default()
            .push(instance_id.as_str());
    }
    let blocked_without_interaction = snapshot
        .blocked_tasks
        .iter()
        .filter(|task| {
            !sessions_by_task
                .get(task.task_id.as_str())
                .is_some_and(|sessions| sessions.iter().any(|id| queued.contains(id)))
        })
        .count();
    let need_you = live_pending.len() + blocked_without_interaction;

    let mut working = 0;
    let mut done_today = 0;
    let mut working_rows: Vec<&ScopeInstance> = Vec::new();
    let mut done_rows: Vec<&ScopeInstance> = Vec::new();
    for instance in &snapshot.instances {
        if instance.done_today {
            done_today += 1;
        }
        if rank_working(&instance.lifecycle, &instance.activity) {
            working += 1;
            working_rows.push(instance);
        } else if rank_done_unseen(instance) && shows_done_unseen(instance, &snapshot) {
            done_rows.push(instance);
        }
    }
    // ◉ and ⑤ are one set (home.md §7): the count is these rows, and ⑤ lists
    // their ids, newest first (deterministic id tiebreak).
    done_rows.sort_by(|a, b| {
        (b.updated_at.as_str(), a.instance_id.as_str())
            .cmp(&(a.updated_at.as_str(), b.instance_id.as_str()))
    });
    let done_unseen = u32::try_from(done_rows.len()).unwrap_or(u32::MAX);
    let done_unseen_ids: Vec<String> = done_rows
        .iter()
        .take(OWNER_GLANCE_ROW_CAP)
        .map(|row| row.instance_id.clone())
        .collect();

    // Waiting rows: one per SESSION IN THE OWNER SCOPE with a live pending
    // interaction, newest interaction first (the inbox queue's order). The
    // count above is unscoped; a row is only painted for the owner's world.
    let scope: std::collections::HashSet<&str> =
        snapshot.scope.iter().map(String::as_str).collect();
    let mut by_instance: HashMap<&str, Vec<&PendingInteraction>> = HashMap::new();
    for item in &pending {
        if item.expired || !scope.contains(item.record.instance_id.as_str()) {
            continue;
        }
        by_instance
            .entry(item.record.instance_id.as_str())
            .or_default()
            .push(item);
    }
    let mut waiting_pairs: Vec<(&str, &&PendingInteraction)> = by_instance
        .iter()
        .filter_map(|(instance, rows)| {
            let newest = rows.iter().max_by(|a, b| {
                (
                    a.record.created_at.as_str(),
                    a.record.interaction_id.as_str(),
                )
                    .cmp(&(
                        b.record.created_at.as_str(),
                        b.record.interaction_id.as_str(),
                    ))
            })?;
            Some((*instance, newest))
        })
        .collect();
    waiting_pairs.sort_by(|(a_id, a), (b_id, b)| {
        (b.record.created_at.as_str(), *a_id).cmp(&(a.record.created_at.as_str(), *b_id))
    });
    let waiting_total = waiting_pairs.len();
    let waiting: Vec<OwnerGlanceRow> = waiting_pairs
        .into_iter()
        .take(OWNER_GLANCE_ROW_CAP)
        .map(|(instance_id, pending)| {
            let (kind, question) = crate::digest::interaction_question(&pending.record);
            let title = snapshot
                .instances
                .iter()
                .find(|row| row.instance_id == instance_id)
                .and_then(|row| row.title.as_deref())
                .map(|title| {
                    clip(
                        &one_line(&crate::digest::redact_secrets(title)),
                        TITLE_CHARS,
                    )
                });
            let headline = if question.trim().is_empty() {
                headline_text(&kind)
            } else {
                headline_text(&format!("{kind}: {question}"))
            };
            OwnerGlanceRow {
                instance_id: instance_id.to_string(),
                title,
                since: since_secs(&pending.record.created_at, now),
                kind,
                headline,
                task_id: snapshot
                    .instances
                    .iter()
                    .find(|row| row.instance_id == instance_id)
                    .and_then(|row| row.task_id.clone()),
            }
        })
        .collect();

    // Working rows: newest activity first, deterministic id tiebreak.
    working_rows.sort_by(|a, b| {
        (b.updated_at.as_str(), a.instance_id.as_str())
            .cmp(&(a.updated_at.as_str(), b.instance_id.as_str()))
    });
    let working_total = working_rows.len();
    let working_out: Vec<OwnerGlanceRow> = working_rows
        .iter()
        .take(OWNER_GLANCE_ROW_CAP)
        .map(|row| {
            let tail = snapshot.journals.get(&row.instance_id);
            let last_text = tail
                .and_then(|events| crate::digest::last_assistant_text(events))
                .filter(|text| !text.trim().is_empty());
            let turn = tail.and_then(|events| crate::digest::last_turn_result(events));
            let title = row.title.as_deref().map(|title| {
                clip(
                    &one_line(&crate::digest::redact_secrets(title)),
                    TITLE_CHARS,
                )
            });
            let headline = match (last_text, turn) {
                (Some(text), _) => headline_text(&text),
                (None, Some(result)) => headline_text(&format!("turn: {result}")),
                (None, None) => title.clone().unwrap_or_else(|| "运行中".to_string()),
            };
            OwnerGlanceRow {
                instance_id: row.instance_id.clone(),
                title,
                since: since_secs(&row.updated_at, now),
                kind: "working".to_string(),
                headline,
                task_id: row.task_id.clone(),
            }
        })
        .collect();

    OwnerGlance {
        generated_at: crate::config::rfc3339(OffsetDateTime::now_utc()),
        timezone_offset_minutes: offset_minutes,
        counts: OwnerGlanceCounts {
            need_you: u32::try_from(need_you).unwrap_or(u32::MAX),
            done_unseen,
            working: u32::try_from(working).unwrap_or(u32::MAX),
            done_today: u32::try_from(done_today).unwrap_or(u32::MAX),
        },
        done_unseen_ids,
        more: GlanceMore {
            waiting: u32::try_from(waiting_total.saturating_sub(waiting.len())).unwrap_or(u32::MAX),
            working: u32::try_from(working_total.saturating_sub(working_out.len()))
                .unwrap_or(u32::MAX),
        },
        waiting,
        working: working_out,
    }
}

/// §7: only Main's current chapter shows ◉ among the Main lineage; a session
/// outside that lineage counts whenever it ranks done-unseen.
fn shows_done_unseen(instance: &ScopeInstance, snapshot: &OwnerGlanceSnapshot) -> bool {
    if !instance.in_main_lineage {
        return true;
    }
    snapshot.main_instance.as_deref() == Some(instance.instance_id.as_str())
}

fn since_secs(stamp: &str, now: i64) -> Option<i64> {
    OffsetDateTime::parse(stamp, &time::format_description::well_known::Rfc3339)
        .map(|at| (now - at.unix_timestamp()).max(0))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: i64 = 1_793_236_200; // 2026-10-29T01:10:00Z

    fn instance(id: &str, lifecycle: &str, activity: &str) -> ScopeInstance {
        ScopeInstance {
            instance_id: id.to_string(),
            title: Some(format!("session {id}")),
            lifecycle: lifecycle.to_string(),
            activity: activity.to_string(),
            updated_at: "2026-10-29T01:00:00.000Z".to_string(),
            task_id: None,
            has_turn_error: false,
            in_main_lineage: false,
            unseen: false,
            done_today: false,
        }
    }

    fn pending(id: &str, instance_id: &str, request: Value) -> PendingInteraction {
        PendingInteraction {
            record: InteractionRecord {
                interaction_id: id.to_string(),
                instance_id: instance_id.to_string(),
                host_id: "hst_fixture".to_string(),
                kind: "permission".to_string(),
                state: "pending".to_string(),
                blocking: true,
                payload: json!({ "payload": { "interaction": {
                    "id": id, "state": "pending", "request": request,
                    "deadline": { "state": "unknown" }
                }}}),
                created_at: "2026-10-29T01:00:00.000Z".to_string(),
                updated_at: "2026-10-29T01:00:00.000Z".to_string(),
            },
            expired: false,
        }
    }

    fn snapshot(instances: Vec<ScopeInstance>) -> OwnerGlanceSnapshot {
        let task_sessions = instances
            .iter()
            .filter_map(|row| {
                row.task_id
                    .as_ref()
                    .map(|task| (row.instance_id.clone(), task.clone()))
            })
            .collect();
        OwnerGlanceSnapshot {
            scope: instances
                .iter()
                .map(|row| row.instance_id.clone())
                .collect(),
            instances,
            blocked_tasks: Vec::new(),
            task_sessions,
            journals: HashMap::new(),
            main_instance: None,
        }
    }

    fn render(snapshot: OwnerGlanceSnapshot) -> OwnerGlance {
        render_with(snapshot, Vec::new())
    }

    fn render_with(snapshot: OwnerGlanceSnapshot, pending: Vec<PendingInteraction>) -> OwnerGlance {
        render_owner_glance(snapshot, pending, NOW, 0)
    }

    /// The ONE Main resolution (home.md §5.3/§6.1): the Hub runs the same
    /// cases the client's `resolveMain` test runs, from the same file.
    #[test]
    fn the_main_seat_follows_the_shared_cases() {
        #[derive(Deserialize)]
        struct Case {
            name: String,
            seats: Vec<MainSeatRow>,
            expect: Option<String>,
        }
        #[derive(Deserialize)]
        struct Cases {
            cases: Vec<Case>,
        }
        let cases: Cases =
            serde_json::from_str(include_str!("../tests/fixtures/main_seat_cases.json"))
                .expect("main seat cases");
        assert!(cases.cases.len() >= 10, "the fixture is the contract");
        for case in &cases.cases {
            let picked = pick_main_seat(&case.seats).map(|seat| seat.instance_id.clone());
            assert_eq!(picked, case.expect, "{}", case.name);
            // Order-independent: the rule is a total order, never "first read".
            let mut reversed = case.seats.clone();
            reversed.reverse();
            let again = pick_main_seat(&reversed).map(|seat| seat.instance_id.clone());
            assert_eq!(again, case.expect, "{} (reversed)", case.name);
        }
    }

    #[test]
    fn the_scope_count_rules_read_one_rank_table() {
        assert!(rank_working("running", "working"));
        assert!(rank_working("running", "draining"));
        // A live row whose activity the Hub has not classified is working.
        assert!(rank_working("ready", "unknown"));
        // Starting and ended ranks are neither working nor done-unseen.
        assert!(!rank_working("starting", "working"));
        assert!(!rank_working("requested", "idle"));
        assert!(!rank_working("exited", "idle"));
        // A pending interaction is rank 1, never rank 3.
        assert!(!rank_working("running", "blocked"));
        assert!(!rank_working("running", "waiting-interaction"));
    }

    #[test]
    fn a_failed_turn_is_a_queue_card_not_done_unseen() {
        let mut failed = instance("ins_failed", "running", "idle");
        failed.unseen = true;
        failed.has_turn_error = true;
        let mut unseen = instance("ins_unseen", "running", "idle");
        unseen.unseen = true;
        let mut opened = instance("ins_opened", "running", "idle");
        opened.unseen = true;
        opened.in_main_lineage = true;
        let mut older_chapter = instance("ins_older", "running", "idle");
        older_chapter.unseen = true;
        older_chapter.in_main_lineage = true;
        let mut snap = snapshot(vec![failed, unseen, opened, older_chapter]);
        snap.main_instance = Some("ins_opened".to_string());
        let glance = render(snap);
        // ◉ counts the Main chapter and the ordinary session, never the failed
        // turn nor Main's fenced predecessor (§7).
        assert_eq!(glance.counts.done_unseen, 2);
        // ⑤ lists exactly that set (same updated_at: id order).
        assert_eq!(glance.done_unseen_ids, ["ins_opened", "ins_unseen"]);
    }

    /// [r4 item 3] ⑤ lists exactly what ◉ counts: newest first, a child
    /// included, capped at the row cap while the count stays the total.
    #[test]
    fn done_unseen_ids_are_the_counted_set_newest_first() {
        let mut rows = Vec::new();
        for index in 0..(OWNER_GLANCE_ROW_CAP + 2) {
            let mut row = instance(&format!("ins_{index:03}"), "running", "idle");
            row.unseen = true;
            row.updated_at = format!("2026-10-29T00:{:02}:00.000Z", index % 60);
            rows.push(row);
        }
        // Not ◉: seen, or still working.
        rows.push(instance("ins_seen", "running", "idle"));
        let mut working = instance("ins_working", "running", "working");
        working.unseen = true;
        rows.push(working);
        let glance = render(snapshot(rows));
        let total = OWNER_GLANCE_ROW_CAP + 2;
        assert_eq!(glance.counts.done_unseen as usize, total);
        assert_eq!(glance.done_unseen_ids.len(), OWNER_GLANCE_ROW_CAP);
        let newest = format!("ins_{:03}", total - 1);
        assert_eq!(glance.done_unseen_ids[0], newest);
        assert!(
            !glance
                .done_unseen_ids
                .iter()
                .any(|id| id == "ins_seen" || id == "ins_working")
        );
    }

    #[test]
    fn need_you_is_the_queue_plus_tasks_without_an_interaction() {
        let mut a = instance("ins_a", "running", "blocked");
        a.task_id = Some("tsk_a".to_string());
        let mut b = instance("ins_b", "running", "blocked");
        b.task_id = Some("tsk_b".to_string());
        let mut snap = snapshot(vec![a, b]);
        // Two blocked tasks; the one whose session is already queued is not
        // counted twice (§5.1 B).
        snap.blocked_tasks = vec![
            BlockedTask {
                task_id: "tsk_a".to_string(),
            },
            BlockedTask {
                task_id: "tsk_b".to_string(),
            },
        ];
        let glance = render_with(
            snap,
            vec![pending(
                "int_a",
                "ins_a",
                json!({ "kind": "approval", "title": "Bash", "description": "run it?" }),
            )],
        );
        assert_eq!(glance.counts.need_you, 2);
        assert_eq!(glance.counts.working, 0);
    }

    #[test]
    fn an_expired_interaction_is_not_queued_and_not_counted() {
        let snap = snapshot(vec![instance("ins_a", "running", "blocked")]);
        let expired = PendingInteraction {
            record: pending(
                "int_a",
                "ins_a",
                json!({ "kind": "approval", "title": "Bash" }),
            )
            .record,
            expired: true,
        };
        let glance = render_with(snap, vec![expired]);
        assert_eq!(glance.counts.need_you, 0);
        assert!(glance.waiting.is_empty());
    }

    #[test]
    fn a_deadline_in_the_past_marks_the_interaction_expired() {
        let mut row = pending(
            "int_a",
            "ins_a",
            json!({ "kind": "approval", "title": "Bash" }),
        );
        row.record.payload = json!({ "payload": { "interaction": {
            "id": "int_a", "state": "pending",
            "request": { "kind": "approval", "title": "Bash" },
            "deadline": { "state": "known", "value": "2026-10-29T00:00:00.000Z" }
        }}});
        assert!(interaction_deadline_passed(&row.record, NOW));
        row.record.payload = json!({ "payload": { "interaction": {
            "id": "int_a", "state": "pending",
            "request": { "kind": "approval", "title": "Bash" },
            "deadline": { "state": "known", "value": "2026-10-29T02:00:00.000Z" }
        }}});
        assert!(!interaction_deadline_passed(&row.record, NOW));
    }

    #[test]
    fn the_rows_are_capped_at_fifty_with_the_overflow_counted() {
        let mut instances = Vec::new();
        let mut pending_rows = Vec::new();
        for index in 0..55 {
            let id = format!("ins_{index:02}");
            instances.push(instance(&id, "running", "working"));
            pending_rows.push(pending(
                &format!("int_{index:02}"),
                &id,
                json!({ "kind": "approval", "title": "Bash", "description": "go?" }),
            ));
        }
        let snap = snapshot(instances);
        let glance = render_with(snap, pending_rows);
        assert_eq!(glance.waiting.len(), OWNER_GLANCE_ROW_CAP);
        assert_eq!(glance.working.len(), OWNER_GLANCE_ROW_CAP);
        assert_eq!(glance.more.waiting, 5);
        assert_eq!(glance.more.working, 5);
        // The count is the whole set, not the page.
        assert_eq!(glance.counts.need_you, 55);
        assert_eq!(glance.counts.working, 55);
    }

    #[test]
    fn headlines_are_deterministic_and_redacted() {
        // Assembled at run time: no line of this source may read as a token.
        let token = format!("{}{}", "sk-", "ant-api03-QWERTYuiop1234567890");
        let mut row = instance("ins_a", "running", "blocked");
        row.title = Some(format!("lease {token} session"));
        let snap = snapshot(vec![row]);
        let first = render_with(
            snap,
            vec![pending(
                "int_a",
                "ins_a",
                json!({ "kind": "approval", "title": "Bash",
                         "description": format!("run with {token} now") }),
            )],
        );
        let again = render_with(
            snapshot(vec![instance("ins_a", "running", "blocked")]),
            vec![pending(
                "int_a",
                "ins_a",
                json!({ "kind": "approval", "title": "Bash",
                         "description": format!("run with {token} now") }),
            )],
        );
        let headline = &first.waiting[0].headline;
        assert!(headline.starts_with("approval: "), "{headline}");
        assert!(!headline.contains(&token), "{headline}");
        assert_eq!(headline, &again.waiting[0].headline);
        assert!(
            first.waiting[0]
                .title
                .as_deref()
                .is_some_and(|t| !t.contains(&token))
        );
    }

    #[test]
    fn a_working_headline_prefers_the_assistant_tail_and_never_a_secret() {
        let token = format!("{}{}", "ghp_", "TESTaaaa1111bbbb2222cccc3333");
        let mut snap = snapshot(vec![instance("ins_a", "running", "working")]);
        snap.journals.insert(
            "ins_a".to_string(),
            vec![
                json!({ "kind": "message", "observedAt": "2026-10-29T01:05:00.000Z",
                "payload": { "operation": "open", "messageId": "m1", "role": "assistant",
                             "parentToolCallId": null,
                             "blocks": [{ "type": "text",
                                          "text": format!("pushing the fix with {token}") }] } }),
            ],
        );
        let glance = render(snap);
        let headline = &glance.working[0].headline;
        assert_eq!(headline, "pushing the fix with [redacted]");
    }

    #[test]
    fn a_working_row_without_text_falls_back_to_the_turn_result() {
        let mut snap = snapshot(vec![instance("ins_a", "running", "working")]);
        snap.journals.insert(
            "ins_a".to_string(),
            vec![json!({ "kind": "lifecycle", "payload": { "type": "native",
                "topic": "turn", "nativeName": "result",
                "status": { "state": "known", "value": "turn_done" } } })],
        );
        let glance = render(snap);
        assert_eq!(glance.working[0].headline, "turn: turn_done");
    }

    /// §5.1: ① needYou is the ④ header's / OS badge's count — derived over
    /// EVERY interaction, not just the owner scope. §5.3 rows stay scoped.
    #[test]
    fn an_out_of_scope_pending_counts_but_paints_no_row() {
        let snap = snapshot(vec![instance("ins_owner", "running", "working")]);
        let glance = render_with(
            snap,
            vec![pending(
                "int_private",
                "ins_private_lineage",
                json!({ "kind": "approval", "title": "Bash" }),
            )],
        );
        assert_eq!(glance.counts.need_you, 1);
        assert!(
            glance.waiting.is_empty(),
            "the row is outside the owner scope"
        );
    }

    /// The two pages must be the SAME set: the store reads journal tails for
    /// the truncated page, and the renderer paints that page. Past 50 working
    /// sessions an id-sorted read used to miss the displayed (updated_at
    /// sorted) rows.
    #[tokio::test]
    async fn past_fifty_working_sessions_the_painted_rows_have_their_headlines() {
        let dir = tempfile::tempdir().unwrap();
        let (store, host) = crate::store_test_support::open_with_host(dir.path(), "glance-60")
            .await
            .unwrap();
        for index in 0..60 {
            let id = format!("ins-glance-{index:02}");
            store
                .ensure_instance(host.clone(), id.clone())
                .await
                .unwrap();
            // A hand-opened root (§5.3): the owner glance lists only rows the
            // Hub stamped as Human-created.
            store
                .run_named("seed_working", {
                    let id = id.clone();
                    move |conn| {
                        conn.execute(
                            "UPDATE instances SET creator_origin = 'human' WHERE id = ?1",
                            params![id],
                        )?;
                        Ok(())
                    }
                })
                .await
                .unwrap();
            store
                .append_journal(
                    host.clone(),
                    id,
                    None,
                    json!({
                        "kind": "lifecycle",
                        "source": { "driverKind": "shell-pty", "channel": "hook" },
                        "payload": { "type": "native", "nativeName": "PreToolUse",
                                     "status": { "state": "known", "value": "working" },
                                     "relatedIds": { "remudaActivity": "working" } }
                    }),
                )
                .await
                .unwrap();
        }
        let now = OffsetDateTime::now_utc();
        let day_start = crate::config::rfc3339(local_day_start(now, 0));
        let now_str = crate::config::rfc3339(now);
        let snapshot = store
            .owner_glance_snapshot(day_start, now_str)
            .await
            .unwrap();
        let read: std::collections::BTreeSet<String> = snapshot.journals.keys().cloned().collect();
        assert_eq!(read.len(), OWNER_GLANCE_ROW_CAP);
        let glance = render_owner_glance(snapshot, Vec::new(), now.unix_timestamp(), 0);
        let painted: std::collections::BTreeSet<String> = glance
            .working
            .iter()
            .map(|row| row.instance_id.clone())
            .collect();
        assert_eq!(painted, read, "the tail set is exactly the displayed page");
        assert!(glance.working.iter().all(|row| !row.headline.is_empty()));
        assert_eq!(glance.counts.working, 60);
        store.close().await;
    }

    /// §5.1 A counts interactions, not sessions: two cards on one session are
    /// two 需要你 and one waiting ROW (§5.3 is per-session).
    #[test]
    fn two_pending_cards_on_one_session_count_twice_but_row_once() {
        let snap = snapshot(vec![instance("ins_a", "running", "blocked")]);
        let glance = render_with(
            snap,
            vec![
                pending(
                    "int_a",
                    "ins_a",
                    json!({ "kind": "approval", "title": "Bash" }),
                ),
                pending(
                    "int_b",
                    "ins_a",
                    json!({ "kind": "approval", "title": "Read" }),
                ),
            ],
        );
        assert_eq!(glance.counts.need_you, 2);
        assert_eq!(glance.waiting.len(), 1);
    }

    /// The `/v1/interactions` page merges each Node's live copy and the Hub's
    /// in-memory grants; the owner count must see the same rows (the e2e fake
    /// Node parks its approvals in the live copy, not in the table).
    #[test]
    fn a_merged_node_card_becomes_a_pending_interaction() {
        let card = json!({
            "id": "int_broker", "interactionId": "int_broker",
            "instanceId": "ins_a", "hostId": "hst_1", "kind": "approval",
            "state": "pending", "blocking": true,
            "createdAt": "2026-10-29T01:00:00.000Z",
            "updatedAt": "2026-10-29T01:00:00.000Z",
            "request": { "kind": "approval", "title": "Bash", "description": "echo e2e" },
            "deadline": { "state": "unknown" },
        });
        let pending = merged_item_pending(&card, NOW).expect("a broker card");
        assert_eq!(pending.record.instance_id, "ins_a");
        assert!(!pending.expired);
        let (kind, question) = crate::digest::interaction_question(&pending.record);
        assert_eq!(kind, "approval");
        assert_eq!(question, "echo e2e");
    }

    /// §5.3: hand-opened is defined from the Hub-stamped creator — the row (or
    /// the ROOT of its resume/continuation chain) was created by a Human
    /// device. Bot roots, agent-lineage successors and stamp-less rows are out.
    #[tokio::test]
    async fn hand_opened_follows_the_root_creator() {
        let dir = tempfile::tempdir().unwrap();
        let (store, host) = crate::store_test_support::open_with_host(dir.path(), "glance-hand")
            .await
            .unwrap();
        let seeds: Vec<(&str, Option<&str>, Value, &str, i64)> = vec![
            (
                "ins.human_root",
                Some("human"),
                json!({}),
                "ins.human_root",
                1,
            ),
            ("ins.bot_root", Some("bot"), json!({}), "ins.bot_root", 1),
            (
                "ins.human_root.child",
                Some("agent"),
                json!({ "parentInstanceId": "ins.human_root" }),
                "ins.human_root.child",
                1,
            ),
            (
                // A plain D-026 resume, as http.rs writes it: the resumed
                // session is BOTH `parentInstanceId` and `resumedFrom`, and
                // the stamp is inherited. It is NOT a true root — it joins
                // through the chain.
                "ins.human_root.resumed",
                Some("human"),
                json!({ "parentInstanceId": "ins.human_root",
                        "resumedFrom": "ins.human_root" }),
                "ins.human_root.resumed",
                1,
            ),
            ("ins.phantom", None, json!({}), "ins.phantom", 1),
            (
                "ins.worker_root",
                Some("agent"),
                json!({}),
                "ins.worker_root",
                1,
            ),
            (
                // A successor in an Agent-dispatched worker's chain. The Hub
                // stamps it `agent` (inherited); this row is deliberately
                // mis-stamped `human` to pin that scope reads the ROOT's
                // stamp, never the row's own.
                "ins.worker_root.successor",
                Some("human"),
                json!({ "parentInstanceId": "ins.worker_root",
                        "resumedFrom": "ins.worker_root" }),
                "ins.worker_root",
                2,
            ),
            // Upgrade backfill's remaining true roots are accepted as
            // hand-opened.
            (
                "ins.legacy_root",
                Some("legacy"),
                json!({}),
                "ins.legacy_root",
                1,
            ),
            // The owner resumes a Bot-created root (D-026): a resume, never a
            // true root, whose ROOT is the Bot session. Deliberately stamped
            // `human` (the Hub inherits `bot`) for the same reason as above.
            (
                "ins.bot_root.relaunch",
                Some("human"),
                json!({ "parentInstanceId": "ins.bot_root",
                        "resumedFrom": "ins.bot_root" }),
                "ins.bot_root.relaunch",
                1,
            ),
            // The SECOND continuation of a hand-opened lineage is driven
            // through the real resume and C1 restart paths in
            // tests/restart.rs (`a_twice_continued_hand_opened_lineage_…`).
        ];
        for (id, creator, spec, lineage, generation) in seeds {
            store
                .ensure_instance(host.clone(), id.to_string())
                .await
                .unwrap();
            store
                .run_named("seed_hand", move |conn| {
                    conn.execute(
                        "UPDATE instances
                            SET creator_origin = ?2, spec_json = ?3,
                                lineage_id = ?4, generation = ?5
                          WHERE id = ?1",
                        params![id, creator, spec.to_string(), lineage, generation],
                    )?;
                    Ok(())
                })
                .await
                .unwrap();
        }
        let now = OffsetDateTime::now_utc();
        let day_start = crate::config::rfc3339(local_day_start(now, 0));
        let now_str = crate::config::rfc3339(now);
        let snapshot = store
            .owner_glance_snapshot(day_start, now_str)
            .await
            .unwrap();
        let scope: std::collections::BTreeSet<&str> =
            snapshot.scope.iter().map(String::as_str).collect();
        assert!(scope.contains("ins.human_root"), "a Human-created root");
        assert!(
            scope.contains("ins.human_root.resumed"),
            "a resume whose root is hand-opened"
        );
        assert!(!scope.contains("ins.bot_root"), "a Bot-created root");
        assert!(
            !scope.contains("ins.human_root.child"),
            "a hand-opened session's dispatched child is its private lineage"
        );
        assert!(!scope.contains("ins.phantom"), "a stamp-less row");
        assert!(
            !scope.contains("ins.worker_root.successor"),
            "a successor whose root is an Agent-created worker"
        );
        assert!(
            scope.contains("ins.legacy_root"),
            "an upgraded true root the backfill calls legacy is hand-opened"
        );
        assert!(
            !scope.contains("ins.bot_root.relaunch"),
            "a human resume of a Bot root is not hand-opened"
        );
        store.close().await;
    }

    /// [r3 item 1] A pre-stamp database has every `creator_origin` NULL, so
    /// without a backfill every hand-opened session vanishes from ①/⑥ and a
    /// Hub without Main shows nothing. The unconditional opener classifies
    /// each row from durable evidence (dispatch audit/parent → agent, a
    /// Human seat → human, a remaining true root → legacy).
    #[tokio::test]
    async fn an_upgraded_database_backfills_the_creator_stamp() {
        use crate::store::Store;

        let dir = tempfile::tempdir().unwrap();
        let (store, host) =
            crate::store_test_support::open_with_host(dir.path(), "glance-backfill")
                .await
                .unwrap();
        let seeds = [
            ("ins.old_root", json!({})),
            (
                "ins.old_child",
                json!({ "parentInstanceId": "ins.old_root" }),
            ),
            // Pre-upgrade D-026 resumes: the Hub wrote `parentInstanceId`
            // AND `resumedFrom` as the resumed session (http.rs resume). A
            // parent here is NOT an Agent creator, so they inherit instead.
            (
                "ins.old_root.resumed",
                json!({ "parentInstanceId": "ins.old_root",
                        "resumedFrom": "ins.old_root" }),
            ),
            (
                "ins.old_root.resumed.again",
                json!({ "parentInstanceId": "ins.old_root.resumed",
                        "resumedFrom": "ins.old_root.resumed" }),
            ),
            (
                "ins.old_child.resumed",
                json!({ "parentInstanceId": "ins.old_child",
                        "resumedFrom": "ins.old_child" }),
            ),
        ];
        for (id, spec) in seeds {
            store
                .ensure_instance(host.clone(), id.to_string())
                .await
                .unwrap();
            store
                .run_named("seed_old", move |conn| {
                    conn.execute(
                        "UPDATE instances
                            SET spec_json = ?2, creator_origin = NULL, lineage_id = ?1
                          WHERE id = ?1",
                        params![id, spec.to_string()],
                    )?;
                    Ok(())
                })
                .await
                .unwrap();
        }
        store.close().await;

        // The upgraded shape: the column did not exist before this build.
        let db = rusqlite::Connection::open(dir.path().join("hub.sqlite")).unwrap();
        db.execute_batch("ALTER TABLE instances DROP COLUMN creator_origin;")
            .unwrap();
        drop(db);

        let store = Store::open(dir.path()).unwrap();
        let root = store
            .get_instance("ins.old_root".into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            root.creator_origin.as_deref(),
            Some("legacy"),
            "a pre-stamp true root becomes legacy"
        );
        let child = store
            .get_instance("ins.old_child".into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            child.creator_origin.as_deref(),
            Some("agent"),
            "a parented row was Agent-created"
        );
        let origin_of = |id: &'static str| {
            let store = &store;
            async move {
                store
                    .get_instance(id.into())
                    .await
                    .unwrap()
                    .unwrap()
                    .creator_origin
            }
        };
        assert_eq!(
            origin_of("ins.old_root.resumed").await.as_deref(),
            Some("legacy"),
            "a resumed conversation inherits its root's stamp, not `agent`"
        );
        assert_eq!(
            origin_of("ins.old_root.resumed.again").await.as_deref(),
            Some("legacy"),
            "chain by chain, to the root"
        );
        assert_eq!(
            origin_of("ins.old_child.resumed").await.as_deref(),
            Some("agent"),
            "a resumed worker is still the Agent's"
        );

        let now = OffsetDateTime::now_utc();
        let day_start = crate::config::rfc3339(local_day_start(now, 0));
        let now_str = crate::config::rfc3339(now);
        let snapshot = store
            .owner_glance_snapshot(day_start, now_str)
            .await
            .unwrap();
        let scope: std::collections::BTreeSet<&str> =
            snapshot.scope.iter().map(String::as_str).collect();
        assert!(scope.contains("ins.old_root"), "the upgraded hand root");
        assert!(
            scope.contains("ins.old_root.resumed.again"),
            "its twice-resumed conversation"
        );
        assert!(!scope.contains("ins.old_child"), "its dispatched child");
        store.close().await;
    }

    /// §5.3 今日完成: the same instant is yesterday for one offset and today
    /// for another at a local midnight boundary.
    #[test]
    fn done_today_follows_the_callers_local_day() {
        let now = OffsetDateTime::parse(
            "2026-11-04T00:30:00.000Z",
            &time::format_description::well_known::Rfc3339,
        )
        .expect("now");
        let stamp = "2026-11-03T22:00:00.000Z";
        let utc_start = crate::config::rfc3339(local_day_start(now, 0));
        let plus_two = crate::config::rfc3339(local_day_start(now, 120));
        let now_str = crate::config::rfc3339(now);
        assert_eq!(utc_start, "2026-11-04T00:00:00.000Z");
        assert_eq!(plus_two, "2026-11-03T22:00:00.000Z");
        // Not today in UTC, today at UTC+2 — the boundary the client's
        // tzOffsetMinutes moves.
        assert!(!(stamp >= utc_start.as_str() && stamp <= now_str.as_str()));
        assert!(stamp >= plus_two.as_str() && stamp <= now_str.as_str());
    }

    fn turn_event(name: &str, status: &str, settled: bool) -> Value {
        json!({
            "kind": "lifecycle",
            "payload": {
                "type": "native", "topic": "turn", "nativeName": name,
                "nativeId": { "state": "known", "value": "sess-glance-upgrade" },
                "status": { "state": "known", "value": status },
                "relatedIds": if settled { json!({ "settledRootTurn": "true" }) } else { json!({}) },
                "dataRef": null, "severity": "info", "affectsCompletion": false
            }
        })
    }

    async fn settle_turn(store: &Store, host: &str, instance: &str) -> i64 {
        store
            .append_journal(
                host.to_owned(),
                instance.to_owned(),
                None,
                turn_event("turn_started", "working", false),
            )
            .await
            .expect("turn start");
        store
            .append_journal(
                host.to_owned(),
                instance.to_owned(),
                None,
                turn_event("result", "turn_done", true),
            )
            .await
            .expect("turn end")
            .record
            .seq
    }

    /// [CRITICAL] home-glance-d060: `turn_end_at` appeared after the seen
    /// rung was already the top of an existing database, so a rung-only
    /// migration never ran on an upgraded hub.sqlite and every settled-turn
    /// append failed with "no such column". The column is now added by the
    /// unconditional opener; this test rewinds a fresh database to exactly
    /// that upgraded shape and proves appends and the glance still work.
    #[tokio::test]
    async fn an_upgraded_database_gains_turn_end_at_and_still_appends_turns() {
        use crate::store::{SCHEMA_USER_VERSION, Store};

        let dir = tempfile::tempdir().unwrap();
        let (store, host) = crate::store_test_support::open_with_host(dir.path(), "glance-upgrade")
            .await
            .unwrap();
        store
            .ensure_instance(host.clone(), "ins-glance-upgrade".into())
            .await
            .unwrap();
        settle_turn(&store, &host, "ins-glance-upgrade").await;
        store.close().await;

        // The shape an upgraded hub.sqlite has: the ladder already sits at its
        // top rung, and the later column does not exist.
        let db = rusqlite::Connection::open(dir.path().join("hub.sqlite")).unwrap();
        let top: i64 = db
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(top, SCHEMA_USER_VERSION, "fresh database climbs the ladder");
        db.execute_batch("ALTER TABLE instances DROP COLUMN turn_end_at;")
            .unwrap();
        drop(db);

        let store = Store::open(dir.path()).unwrap();
        let db = rusqlite::Connection::open(dir.path().join("hub.sqlite")).unwrap();
        assert!(
            crate::store::column_exists(&db, "instances", "turn_end_at").unwrap(),
            "the unconditional opener restores the column on an upgraded database"
        );
        drop(db);

        // The append that used to fail is fine, and the stamp lands.
        settle_turn(&store, &host, "ins-glance-upgrade").await;
        let now = OffsetDateTime::now_utc();
        let day_start = crate::config::rfc3339(local_day_start(now, 0));
        let now_str = crate::config::rfc3339(now);
        let snapshot = store
            .owner_glance_snapshot(day_start, now_str)
            .await
            .expect("the glance reads an upgraded database");
        // The glance route renders without a 500; the row's stamp is present.
        let _glance = render_owner_glance(snapshot, Vec::new(), now.unix_timestamp(), 0);
        store.close().await;
    }
}
