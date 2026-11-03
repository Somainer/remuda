//! D-060 §11.5 (cb-owner-notify): `remuda owner notify`, its claim rows, the
//! dedupe key, the "to you" notices the PWA renders, and the notify primitive
//! the wake engine calls.
//!
//! - **Who may notify.** `POST /v1/owner/notify` takes an Agent that holds
//!   `address-owner` (the seat that addresses the owner) or the owner's own
//!   Human device. Any other caller (an Agent without the grant, a Bot
//!   credential, whatever instance it names) gets 403 with a reason and a next
//!   step. An Agent call is admitted under the commit-time initiator check
//!   (D-057 §7.3): a fenced or superseded chapter cannot notify.
//! - **Bounds and the secret lint.** The title is one line of at most
//!   [`TITLE_MAX_CHARS`], the body at most [`BODY_MAX_CHARS`], the dedupe key
//!   at most [`DEDUPE_KEY_MAX_CHARS`] of a plain token alphabet. The shared
//!   credential shapes ([`crate::secret_lint`]) run over all three before the
//!   writer job: a hit is a 422 naming the field and the shape, never the text,
//!   and nothing is stored.
//! - **Claim row first, then push.** One writer transaction writes the claim
//!   row (`owner_notices`: lineage, chapter, the chapter's turn, title, body,
//!   dedupe key, created) with its audit row, and queues the push in the D-057
//!   §8.3 outbox under the tag `notice:<id>`. The push goes out after commit;
//!   a crash before it is sent (or before it is marked sent) is resent at boot
//!   under the same tag, so a resend replaces the notification. Per-device
//!   suppression (§8.4) applies: a device following the writing chapter with
//!   its page visible is skipped, because it is reading the card already.
//! - **Dedupe.** A repeat with the same dedupe key in the same scope (the
//!   writer's lineage, or the owner for a Human call) returns the original
//!   row, writes nothing and pushes nothing.
//! - **Repeating failures.** [`notify_failure`] latches a failure kind per
//!   scope: the first occurrence notifies, a repeat while the latch is set only
//!   counts, and [`recover`] clears it. [`notify_wake`] is the wake engine's
//!   entry: it applies a wake-up's `notify` declaration (`never`, `failure`,
//!   `always`) on top of these.
//! - **Notices.** `GET /v1/owner/notices` lists them (`unacked=1` for the home
//!   queue, `instanceId` for one chapter's transcript); `POST
//!   /v1/owner/notices/{id}/ack` is the owner's acknowledgement, on every
//!   device at once and idempotent. A notice acknowledged before its push went
//!   out is not pushed. Both are Human-only: a notice is the owner's, so a
//!   Bot credential (which relays to a chat platform) and an Agent (a seat, or
//!   a Human token narrowed to one) get 403 with a reason and a next step.
//! - **Logs.** Categories and outcomes only, never a title, a body or a key.
//!
//! The tables live in `hub.sqlite`, so `scripts/hub-backup.sh` carries them.

use crate::AppState;
use crate::agent_scope::{CallerAuthority, authenticated_caller, caller, origin};
use crate::auth::require_origin;
use crate::config::now_rfc3339;
use crate::error::{HubError, NextStep, action};
use crate::store::{StoreError, check_initiator, immediate_tx};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use remuda_protocol::InputOrigin;
use remuda_push::PushTag;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

/// Characters a notice title may hold (one line).
pub(crate) const TITLE_MAX_CHARS: usize = 120;
/// Characters a notice body may hold.
pub(crate) const BODY_MAX_CHARS: usize = 2000;
/// Characters a dedupe key may hold.
pub(crate) const DEDUPE_KEY_MAX_CHARS: usize = 128;
/// Characters of the body a push carries; the card shows the whole body.
const PUSH_BODY_CHARS: usize = 160;
/// Notices one list returns at most.
const LIST_MAX: usize = 200;
/// Notices one list returns by default.
const LIST_DEFAULT: usize = 50;
/// The dedupe and latch scope of a notice the owner's own device wrote.
const OWNER_SCOPE: &str = "owner";
/// The address-owner grant, as the wire spells it.
const ADDRESS_OWNER: &str = "address-owner";

/// Idempotent DDL; no `user_version` rung.
pub(crate) fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS owner_notices (
            notice_id TEXT PRIMARY KEY,
            lineage_id TEXT,
            chapter_instance_id TEXT,
            chapter_generation INTEGER,
            turn_seq INTEGER,
            title TEXT NOT NULL,
            body TEXT NOT NULL,
            dedupe_scope TEXT NOT NULL,
            dedupe_key TEXT,
            source TEXT NOT NULL,
            origin TEXT NOT NULL,
            created_at TEXT NOT NULL,
            acked_at TEXT,
            acked_by_device TEXT
         );
         CREATE UNIQUE INDEX IF NOT EXISTS owner_notices_dedupe
            ON owner_notices(dedupe_scope, dedupe_key) WHERE dedupe_key IS NOT NULL;
         CREATE INDEX IF NOT EXISTS owner_notices_unacked
            ON owner_notices(created_at) WHERE acked_at IS NULL;
         CREATE INDEX IF NOT EXISTS owner_notices_chapter
            ON owner_notices(chapter_instance_id, created_at);
         CREATE TABLE IF NOT EXISTS owner_notify_latches (
            scope TEXT NOT NULL,
            failure_kind TEXT NOT NULL,
            notice_id TEXT NOT NULL,
            since TEXT NOT NULL,
            repeats INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (scope, failure_kind)
         );",
    )
}

// ─── the claim ──────────────────────────────────────────────────────────────

/// What wrote a notice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NoticeSource {
    /// `remuda owner notify` (the route).
    Cli,
    /// A wake run's `notify` declaration (§11.5).
    #[cfg_attr(not(test), allow(dead_code))]
    Wake,
}

impl NoticeSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Wake => "wake",
        }
    }
}

/// The chapter a notice is written from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NoticeChapter {
    pub lineage_id: String,
    pub instance_id: String,
    pub generation: i64,
}

/// One notice to claim. `chapter` is `None` for the owner's own device.
#[derive(Clone, Debug)]
pub(crate) struct NoticeInput {
    pub chapter: Option<NoticeChapter>,
    pub title: String,
    pub body: String,
    pub dedupe_key: Option<String>,
    pub source: NoticeSource,
    /// `agent`, `human` or `hub`, for the audit row and the card.
    pub origin: &'static str,
}

impl NoticeInput {
    fn scope(&self) -> &str {
        self.chapter
            .as_ref()
            .map_or(OWNER_SCOPE, |chapter| chapter.lineage_id.as_str())
    }
}

/// One stored notice, as the routes return it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Notice {
    pub notice_id: String,
    pub lineage_id: Option<String>,
    pub chapter_instance_id: Option<String>,
    pub turn_seq: Option<i64>,
    pub title: String,
    pub body: String,
    pub dedupe_key: Option<String>,
    pub source: String,
    pub origin: String,
    pub created_at: String,
    pub acked_at: Option<String>,
}

impl Notice {
    pub(crate) fn json(&self) -> Value {
        json!({
            "noticeId": self.notice_id,
            "lineageId": self.lineage_id,
            "chapterInstanceId": self.chapter_instance_id,
            "turnSeq": self.turn_seq,
            "title": self.title,
            "body": self.body,
            "dedupeKey": self.dedupe_key,
            "source": self.source,
            "origin": self.origin,
            "createdAt": self.created_at,
            "ackedAt": self.acked_at,
        })
    }
}

/// The outcome of a claim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Claimed {
    /// A new row; its push is queued.
    Created(Notice),
    /// The row an earlier call with the same dedupe key wrote; nothing new
    /// was written or queued.
    Existing(Notice),
}

impl Claimed {
    pub(crate) fn notice(&self) -> &Notice {
        match self {
            Self::Created(notice) | Self::Existing(notice) => notice,
        }
    }
}

const NOTICE_COLUMNS: &str = "notice_id, lineage_id, chapter_instance_id, turn_seq, title, body, \
     dedupe_key, source, origin, created_at, acked_at";

fn notice_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Notice> {
    Ok(Notice {
        notice_id: row.get(0)?,
        lineage_id: row.get(1)?,
        chapter_instance_id: row.get(2)?,
        turn_seq: row.get(3)?,
        title: row.get(4)?,
        body: row.get(5)?,
        dedupe_key: row.get(6)?,
        source: row.get(7)?,
        origin: row.get(8)?,
        created_at: row.get(9)?,
        acked_at: row.get(10)?,
    })
}

fn load(conn: &Connection, notice_id: &str) -> Result<Option<Notice>, StoreError> {
    Ok(conn
        .query_row(
            &format!("SELECT {NOTICE_COLUMNS} FROM owner_notices WHERE notice_id = ?1"),
            params![notice_id],
            notice_row,
        )
        .optional()?)
}

/// Where a notice push lands: the home, with the 「告诉你」 card focused
/// (a phone keeps the query through the `/sessions` → `/m` redirect).
pub(crate) fn landing_url(notice_id: &str) -> String {
    format!("/sessions?focus={notice_id}")
}

/// The push body: the notice body's first lines, cut at
/// [`PUSH_BODY_CHARS`]. The card shows the whole body.
fn push_body(body: &str) -> String {
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat.chars().take(PUSH_BODY_CHARS).collect();
    if flat.chars().count() > PUSH_BODY_CHARS {
        out.push('…');
    }
    out
}

/// Claim one notice inside a writer job: with a dedupe key already used in
/// the scope, the original row and nothing else; otherwise the claim row, its
/// audit row and its outbox push, all in the caller's transaction. The push
/// is sent after commit ([`crate::push_route::drive_outbox`]).
pub(crate) fn claim(conn: &Connection, input: &NoticeInput) -> Result<Claimed, StoreError> {
    let scope = input.scope().to_owned();
    if let Some(key) = &input.dedupe_key {
        let existing: Option<Notice> = conn
            .query_row(
                &format!(
                    "SELECT {NOTICE_COLUMNS} FROM owner_notices
                      WHERE dedupe_scope = ?1 AND dedupe_key = ?2"
                ),
                params![scope, key],
                notice_row,
            )
            .optional()?;
        if let Some(existing) = existing {
            return Ok(Claimed::Existing(existing));
        }
    }
    let notice_id = format!("ntc_{}", uuid::Uuid::now_v7());
    let now = now_rfc3339();
    let turn_seq: Option<i64> = match &input.chapter {
        Some(chapter) => conn
            .query_row(
                crate::memory::TURN_STARTED_SQL,
                params![chapter.instance_id],
                |row| row.get(0),
            )
            .optional()?,
        None => None,
    };
    conn.execute(
        "INSERT INTO owner_notices
            (notice_id, lineage_id, chapter_instance_id, chapter_generation, turn_seq, title,
             body, dedupe_scope, dedupe_key, source, origin, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            notice_id,
            input.chapter.as_ref().map(|c| c.lineage_id.as_str()),
            input.chapter.as_ref().map(|c| c.instance_id.as_str()),
            input.chapter.as_ref().map(|c| c.generation),
            turn_seq,
            input.title,
            input.body,
            scope,
            input.dedupe_key,
            input.source.as_str(),
            input.origin,
            now,
        ],
    )?;
    // The audit row (§8.6 style): who, from which chapter, by which path; the
    // text stays in the claim row.
    let initiator = input.chapter.as_ref().map(|chapter| {
        json!({
            "instanceId": chapter.instance_id,
            "lineageId": chapter.lineage_id,
            "generation": chapter.generation,
        })
    });
    conn.execute(
        "INSERT INTO audit_log (device_id, action, subject, detail_json, created_at, initiator_json)
         VALUES (?1, 'owner.notify', ?2, ?3, ?4, ?5)",
        params![
            input
                .chapter
                .as_ref()
                .map_or_else(|| input.origin.to_owned(), |c| c.instance_id.clone()),
            notice_id,
            json!({
                "source": input.source.as_str(),
                "origin": input.origin,
                "dedupeKey": input.dedupe_key.is_some(),
                "lineageId": input.chapter.as_ref().map(|c| c.lineage_id.as_str()),
            })
            .to_string(),
            now,
            initiator.map(|value| value.to_string()),
        ],
    )?;
    let instance_id = input
        .chapter
        .as_ref()
        .map_or("", |chapter| chapter.instance_id.as_str());
    crate::push_route::enqueue(
        conn,
        &crate::push_route::OutboxPush {
            key: &format!("notice:{notice_id}"),
            kind: "notice",
            instance_id,
            tag: PushTag::Notice {
                id: notice_id.clone(),
            },
            title: &input.title,
            body: &push_body(&input.body),
            url: &landing_url(&notice_id),
        },
    )?;
    let notice = load(conn, &notice_id)?
        .ok_or_else(|| StoreError::Id("owner notice vanished in its own transaction".into()))?;
    Ok(Claimed::Created(notice))
}

// ─── repeating failures (§11.5) ─────────────────────────────────────────────

// cb-wake-engine calls this; until it lands only the tests do.
#[cfg_attr(not(test), allow(dead_code))]
/// The outcome of [`notify_failure`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FailureNotice {
    /// The first failure of its kind since the scope last recovered: a
    /// notice was claimed (or found by its dedupe key).
    Notified(Box<Claimed>),
    /// A repeat while the previous failure of the same kind has not
    /// recovered: counted, nothing claimed, nothing pushed.
    Repeat {
        /// The notice the first failure claimed.
        notice_id: String,
        /// Repeats since that first failure, this one included.
        repeats: i64,
    },
}

// cb-wake-engine calls this; until it lands only the tests do.
#[cfg_attr(not(test), allow(dead_code))]
/// A failure of `kind` in `scope` (a wake-up, a lineage): notify only when no
/// earlier failure of that kind is still unrecovered.
pub(crate) fn notify_failure(
    conn: &Connection,
    scope: &str,
    kind: &str,
    input: &NoticeInput,
) -> Result<FailureNotice, StoreError> {
    let latched: Option<String> = conn
        .query_row(
            "SELECT notice_id FROM owner_notify_latches WHERE scope = ?1 AND failure_kind = ?2",
            params![scope, kind],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(notice_id) = latched {
        let repeats: i64 = conn.query_row(
            "UPDATE owner_notify_latches SET repeats = repeats + 1
              WHERE scope = ?1 AND failure_kind = ?2 RETURNING repeats",
            params![scope, kind],
            |row| row.get(0),
        )?;
        return Ok(FailureNotice::Repeat { notice_id, repeats });
    }
    let claimed = claim(conn, input)?;
    conn.execute(
        "INSERT INTO owner_notify_latches (scope, failure_kind, notice_id, since)
         VALUES (?1, ?2, ?3, ?4)",
        params![scope, kind, claimed.notice().notice_id, now_rfc3339()],
    )?;
    Ok(FailureNotice::Notified(Box::new(claimed)))
}

// cb-wake-engine calls this; until it lands only the tests do.
#[cfg_attr(not(test), allow(dead_code))]
/// `scope` recovered: clear its latch for `kind`, or for every kind when
/// `kind` is `None`, so the next failure notifies again. Returns how many
/// latches were cleared.
pub(crate) fn recover(
    conn: &Connection,
    scope: &str,
    kind: Option<&str>,
) -> Result<usize, StoreError> {
    Ok(match kind {
        Some(kind) => conn.execute(
            "DELETE FROM owner_notify_latches WHERE scope = ?1 AND failure_kind = ?2",
            params![scope, kind],
        )?,
        None => conn.execute(
            "DELETE FROM owner_notify_latches WHERE scope = ?1",
            params![scope],
        )?,
    })
}

// cb-wake-engine calls this; until it lands only the tests do.
#[cfg_attr(not(test), allow(dead_code))]
/// A wake-up's `notify` declaration (§11.5); the Hub never picks one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum NotifyPolicy {
    /// Never notify.
    Never,
    /// Notify on a failure, once per unrecovered failure kind.
    Failure,
    /// Notify on every success, and on a failure as `failure` does.
    Always,
}

impl std::str::FromStr for NotifyPolicy {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "never" => Ok(Self::Never),
            "failure" => Ok(Self::Failure),
            "always" => Ok(Self::Always),
            other => Err(format!(
                "unknown notify {other:?}; use never, failure or always"
            )),
        }
    }
}

// cb-wake-engine calls this; until it lands only the tests do.
#[cfg_attr(not(test), allow(dead_code))]
/// How a wake run ended, for its `notify` declaration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WakeOutcome<'a> {
    /// The run succeeded (or was skipped without failing).
    Success,
    /// The run failed; `kind` names the failure (`check-timeout`,
    /// `check-exit`, `delivery`, …) so different failures latch apart.
    Failure { kind: &'a str },
}

// cb-wake-engine calls this; until it lands only the tests do.
#[cfg_attr(not(test), allow(dead_code))]
/// The wake engine's call after a run settles: apply the wake-up's `notify`
/// declaration. A success clears every failure latch of the wake-up (it
/// recovered) and, under `always`, notifies; a failure under `failure` or
/// `always` notifies once per unrecovered kind; `never` notifies nothing.
/// `input.dedupe_key` may carry the run id so a retried result write claims
/// nothing twice. Returns the notice outcome, `None` when nothing was due.
pub(crate) fn notify_wake(
    conn: &Connection,
    wake_id: &str,
    policy: NotifyPolicy,
    outcome: WakeOutcome<'_>,
    input: &NoticeInput,
) -> Result<Option<FailureNotice>, StoreError> {
    let scope = format!("wake:{wake_id}");
    match outcome {
        WakeOutcome::Success => {
            recover(conn, &scope, None)?;
            if policy == NotifyPolicy::Always {
                return Ok(Some(FailureNotice::Notified(Box::new(claim(conn, input)?))));
            }
            Ok(None)
        }
        WakeOutcome::Failure { kind } => match policy {
            NotifyPolicy::Never => Ok(None),
            NotifyPolicy::Failure | NotifyPolicy::Always => {
                Ok(Some(notify_failure(conn, &scope, kind, input)?))
            }
        },
    }
}

/// Acknowledge a notice (idempotent): the first acknowledgement stamps it,
/// later ones return it unchanged. `None` when there is no such notice.
pub(crate) fn ack(
    conn: &Connection,
    notice_id: &str,
    device_id: &str,
) -> Result<Option<Notice>, StoreError> {
    let changed = conn.execute(
        "UPDATE owner_notices SET acked_at = ?2, acked_by_device = ?3
          WHERE notice_id = ?1 AND acked_at IS NULL",
        params![notice_id, now_rfc3339(), device_id],
    )?;
    if changed > 0 {
        conn.execute(
            "INSERT INTO audit_log (device_id, action, subject, detail_json, created_at)
             VALUES (?1, 'owner.notice.ack', ?2, '{}', ?3)",
            params![device_id, notice_id, now_rfc3339()],
        )?;
    }
    load(conn, notice_id)
}

/// Whether the owner already acknowledged notice `notice_id` (the outbox
/// sends no push for it). A read error counts as not acknowledged: a missed
/// skip costs one push, a missed push could cost the owner the notice.
pub(crate) async fn acked(state: &AppState, notice_id: &str) -> bool {
    let id = notice_id.to_owned();
    state
        .store
        .read("owner_notice_acked", move |conn| {
            Ok(conn
                .query_row(
                    "SELECT 1 FROM owner_notices WHERE notice_id = ?1 AND acked_at IS NOT NULL",
                    params![id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some())
        })
        .await
        .unwrap_or(false)
}

/// Which notices a list returns.
#[derive(Clone, Debug, Default)]
pub(crate) struct ListFilter {
    pub unacked: bool,
    pub chapter: Option<String>,
    pub limit: usize,
}

/// Notices, newest first.
pub(crate) fn list(conn: &Connection, filter: &ListFilter) -> Result<Vec<Notice>, StoreError> {
    let limit = i64::try_from(filter.limit.clamp(1, LIST_MAX)).unwrap_or(50);
    let mut stmt = conn.prepare(&format!(
        "SELECT {NOTICE_COLUMNS} FROM owner_notices
          WHERE (?1 = 0 OR acked_at IS NULL)
            AND (?2 IS NULL OR chapter_instance_id = ?2)
          ORDER BY created_at DESC, notice_id DESC LIMIT ?3"
    ))?;
    let rows = stmt
        .query_map(
            params![i64::from(filter.unacked), filter.chapter, limit],
            notice_row,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// ─── validation ─────────────────────────────────────────────────────────────

/// A refusal of the notify route's own (the rest are [`HubError`]s).
#[derive(Debug)]
pub(crate) enum NotifyError {
    /// The body is unusable.
    BadRequest(String),
    /// A field looks like a credential; the text is not echoed.
    SecretRejected {
        field: &'static str,
        rule: &'static str,
    },
    /// Any other refusal.
    Hub(HubError),
}

impl From<HubError> for NotifyError {
    fn from(error: HubError) -> Self {
        Self::Hub(error)
    }
}

impl IntoResponse for NotifyError {
    fn into_response(self) -> Response {
        match self {
            Self::Hub(error) => error.into_response(),
            Self::BadRequest(message) => HubError::BadRequest(message)
                .with_next(
                    "invalid-notice",
                    NextStep::new(
                        action::FIX_REQUEST,
                        format!(
                            "send a one-line title of at most {TITLE_MAX_CHARS} characters, a \
                             body of at most {BODY_MAX_CHARS} characters and, optionally, a \
                             dedupe key of at most {DEDUPE_KEY_MAX_CHARS} characters from \
                             [A-Za-z0-9._:/-]"
                        ),
                    ),
                )
                .into_response(),
            Self::SecretRejected { field, rule } => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "error": format!(
                        "the notice was not stored or sent: its {field} looks like a credential \
                         (shape {rule:?}); the text is not echoed"
                    ),
                    "code": "OWNER_NOTIFY_SECRET_REJECTED",
                    "reason": "secret-rejected",
                    "field": field,
                    "rule": rule,
                    "next": NextStep::new(
                        action::FIX_REQUEST,
                        "remove the credential and notify again; say where the secret is \
                         kept, never the secret itself",
                    )
                    .json(),
                })),
            )
                .into_response(),
        }
    }
}

/// The notify body as the CLI sends it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NotifyBody {
    title: String,
    body: String,
    #[serde(default)]
    dedupe_key: Option<String>,
}

/// A checked notify body.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CheckedNotice {
    pub title: String,
    pub body: String,
    pub dedupe_key: Option<String>,
}

/// Bound the fields and run the secret lint over each (§12.5's shapes). The
/// lint runs first, so a credential is refused as one even in a field that
/// is also too long, and no refusal message ever carries the text.
pub(crate) fn check_notice(
    title: &str,
    body: &str,
    dedupe_key: Option<&str>,
) -> Result<CheckedNotice, NotifyError> {
    for (field, text) in [
        ("title", Some(title)),
        ("body", Some(body)),
        ("dedupeKey", dedupe_key),
    ] {
        if let Some(rule) = text.and_then(crate::secret_lint::first_secret) {
            return Err(NotifyError::SecretRejected { field, rule });
        }
    }
    let title = title.trim();
    if title.is_empty() {
        return Err(NotifyError::BadRequest("title must not be empty".into()));
    }
    if title.contains(['\n', '\r']) {
        return Err(NotifyError::BadRequest("title must be one line".into()));
    }
    let title_chars = title.chars().count();
    if title_chars > TITLE_MAX_CHARS {
        return Err(NotifyError::BadRequest(format!(
            "title is {title_chars} characters; at most {TITLE_MAX_CHARS}"
        )));
    }
    let body = body.trim();
    if body.is_empty() {
        return Err(NotifyError::BadRequest("body must not be empty".into()));
    }
    let body_chars = body.chars().count();
    if body_chars > BODY_MAX_CHARS {
        return Err(NotifyError::BadRequest(format!(
            "body is {body_chars} characters; at most {BODY_MAX_CHARS}"
        )));
    }
    let dedupe_key = match dedupe_key.map(str::trim) {
        None | Some("") => None,
        Some(key) => {
            let plain = key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '-'));
            if key.chars().count() > DEDUPE_KEY_MAX_CHARS || !plain {
                return Err(NotifyError::BadRequest(format!(
                    "dedupeKey must be at most {DEDUPE_KEY_MAX_CHARS} characters from \
                     [A-Za-z0-9._:/-]"
                )));
            }
            Some(key.to_owned())
        }
    };
    Ok(CheckedNotice {
        title: title.to_owned(),
        body: body.to_owned(),
        dedupe_key,
    })
}

/// 403 for a caller that may not notify the owner: an Agent without
/// `address-owner` is told which grant it lacks; a Bot credential is told
/// that only the seat and the owner's own device may notify.
fn not_a_notifier() -> HubError {
    HubError::Forbidden.with_next(
        "address-owner-required",
        NextStep::new(
            action::ASK_OWNER,
            "only the seat holding `address-owner` (Main) or the owner's own device may \
             notify the owner; do not retry — put what the owner should know in a report to \
             your parent, or ask the owner",
        )
        .grant(ADDRESS_OWNER),
    )
}

/// 403 for a notices read or acknowledgement from anything but the owner's
/// own Human device: a Bot relays to a chat platform and an Agent is a seat
/// (a narrowed Human token counts as its Agent), and neither may read the
/// owner's notices or dismiss them from the owner's devices.
fn owner_only() -> HubError {
    HubError::Forbidden.with_next(
        "owner-only",
        NextStep::new(
            action::ASK_OWNER,
            "owner notices are read and acknowledged only from the owner's own device; do \
             not retry with this credential",
        ),
    )
}

/// The owner's own Human device, or [`owner_only`].
async fn owner_device(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<crate::store::Device, HubError> {
    require_origin(headers, &state.config)?;
    let device = caller(state, headers).await?;
    if origin(&device) != InputOrigin::Human {
        tracing::info!(category = "owner-notice", outcome = "refused-not-human");
        return Err(owner_only());
    }
    Ok(device)
}

// ─── routes ─────────────────────────────────────────────────────────────────

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/owner/notify", post(notify))
        .route("/v1/owner/notices", get(list_notices))
        .route("/v1/owner/notices/{id}/ack", post(ack_notice))
}

async fn notify(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<NotifyBody>,
) -> Result<Response, NotifyError> {
    require_origin(&headers, &state.config)?;
    let (authenticated, device) = authenticated_caller(&state, &headers).await?;
    // A Bot relays other people's words; it is never the seat, whatever
    // instance it names (the narrowed caller alone would let it act as one).
    if origin(&authenticated) == InputOrigin::Bot {
        tracing::info!(category = "owner-notify", outcome = "refused-bot");
        return Err(not_a_notifier().into());
    }
    let caller_origin = origin(&device);
    let authority = match caller_origin {
        InputOrigin::Human => CallerAuthority::internal(),
        InputOrigin::Agent => {
            let instance_id = device.instance_id.clone().ok_or_else(not_a_notifier)?;
            let instance = state
                .store
                .get_instance(instance_id)
                .await
                .map_err(crate::http::map_store)?
                .ok_or_else(HubError::stale_caller)?;
            if !instance.grants.iter().any(|grant| grant == ADDRESS_OWNER) {
                tracing::info!(category = "owner-notify", outcome = "refused-grant");
                return Err(HubError::missing_grant(ADDRESS_OWNER).into());
            }
            CallerAuthority::for_device(&state, &device).await?
        }
        _ => return Err(not_a_notifier().into()),
    };
    let checked = check_notice(&body.title, &body.body, body.dedupe_key.as_deref())?;
    let chapter = authority.initiator.as_ref().map(|initiator| NoticeChapter {
        lineage_id: initiator.lineage_id.clone(),
        instance_id: initiator.instance_id.clone(),
        generation: initiator.generation,
    });
    let input = NoticeInput {
        chapter,
        title: checked.title,
        body: checked.body,
        dedupe_key: checked.dedupe_key,
        source: NoticeSource::Cli,
        origin: if caller_origin == InputOrigin::Human {
            "human"
        } else {
            "agent"
        },
    };
    let claimed = state
        .store
        .run_named("owner_notify", move |conn| {
            let tx = immediate_tx(conn)?;
            // D-057 §7.3: a fenced or superseded chapter writes nothing.
            let (initiator, device_id) = authority.as_check();
            check_initiator(&tx, initiator, device_id)?;
            let claimed = claim(&tx, &input)?;
            tx.commit()?;
            Ok(claimed)
        })
        .await
        .map_err(crate::http::map_store)?;
    let (status, created) = match &claimed {
        Claimed::Created(_) => (StatusCode::CREATED, true),
        Claimed::Existing(_) => (StatusCode::OK, false),
    };
    tracing::info!(
        category = "owner-notify",
        outcome = if created { "claimed" } else { "deduped" },
        caller = if caller_origin == InputOrigin::Human {
            "human"
        } else {
            "agent"
        },
    );
    Ok((
        status,
        Json(json!({ "notice": claimed.notice().json(), "created": created })),
    )
        .into_response())
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListQuery {
    #[serde(default)]
    unacked: Option<String>,
    #[serde(default)]
    instance_id: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

async fn list_notices(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, HubError> {
    owner_device(&state, &headers).await?;
    let filter = ListFilter {
        unacked: matches!(query.unacked.as_deref(), Some("1" | "true")),
        chapter: query.instance_id.filter(|id| !id.is_empty()),
        limit: query.limit.unwrap_or(LIST_DEFAULT),
    };
    let notices = state
        .store
        .read("owner_notices", move |conn| list(conn, &filter))
        .await
        .map_err(crate::http::map_store)?;
    Ok(Json(json!({
        "items": notices.iter().map(Notice::json).collect::<Vec<_>>(),
    })))
}

async fn ack_notice(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, HubError> {
    let device = owner_device(&state, &headers).await?;
    let notice = state
        .store
        .run_named("owner_notice_ack", move |conn| ack(conn, &id, &device.id))
        .await
        .map_err(crate::http::map_store)?
        .ok_or(HubError::NotFound)?;
    tracing::info!(category = "owner-notice", outcome = "acked");
    Ok(Json(json!({ "notice": notice.json() })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().expect("db");
        conn.execute_batch(
            "CREATE TABLE audit_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT, device_id TEXT NOT NULL,
                action TEXT NOT NULL, subject TEXT, detail_json TEXT NOT NULL DEFAULT '{}',
                created_at TEXT NOT NULL, initiator_json TEXT);
             CREATE TABLE journal (instance_id TEXT, seq INTEGER, payload_json TEXT,
                PRIMARY KEY (instance_id, seq));",
        )
        .expect("base tables");
        crate::push_route::migrate(&conn).expect("push tables");
        migrate(&conn).expect("notice tables");
        conn
    }

    fn input(title: &str, key: Option<&str>) -> NoticeInput {
        NoticeInput {
            chapter: Some(NoticeChapter {
                lineage_id: "ins_main".into(),
                instance_id: "ins_main".into(),
                generation: 1,
            }),
            title: title.into(),
            body: "The nightly gate failed twice.".into(),
            dedupe_key: key.map(str::to_owned),
            source: NoticeSource::Wake,
            origin: "hub",
        }
    }

    fn outbox_rows(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM push_outbox WHERE kind = 'notice'",
            [],
            |row| row.get(0),
        )
        .expect("count")
    }

    #[test]
    fn a_repeated_dedupe_key_returns_the_original_row_and_queues_nothing() {
        let conn = db();
        let first = claim(&conn, &input("Gate red", Some("gate:nightly"))).expect("claim");
        let Claimed::Created(original) = first else {
            panic!("first claim must create");
        };
        let again = claim(&conn, &input("Gate red again", Some("gate:nightly"))).expect("claim");
        assert_eq!(again, Claimed::Existing(original.clone()));
        assert_eq!(outbox_rows(&conn), 1);
        let tag: String = conn
            .query_row(
                "SELECT tag_kind || ':' || tag_id FROM push_outbox",
                [],
                |row| row.get(0),
            )
            .expect("tag");
        assert_eq!(tag, format!("notice:{}", original.notice_id));
        // Another lineage's key space is its own.
        let mut other = input("Gate red", Some("gate:nightly"));
        other.chapter.as_mut().expect("chapter").lineage_id = "ins_other".into();
        assert!(matches!(
            claim(&conn, &other).expect("claim"),
            Claimed::Created(_)
        ));
    }

    #[test]
    fn a_repeating_failure_pushes_once_until_it_recovers() {
        let conn = db();
        let failed = |conn: &Connection| {
            notify_wake(
                conn,
                "wak_1",
                NotifyPolicy::Failure,
                WakeOutcome::Failure {
                    kind: "check-timeout",
                },
                &input("Wake check timed out", None),
            )
            .expect("wake")
        };
        let Some(FailureNotice::Notified(claimed)) = failed(&conn) else {
            panic!("the first failure notifies");
        };
        let Claimed::Created(first) = *claimed else {
            panic!("the first failure claims a new notice");
        };
        assert_eq!(
            failed(&conn),
            Some(FailureNotice::Repeat {
                notice_id: first.notice_id.clone(),
                repeats: 1
            })
        );
        assert_eq!(
            failed(&conn),
            Some(FailureNotice::Repeat {
                notice_id: first.notice_id.clone(),
                repeats: 2
            })
        );
        assert_eq!(outbox_rows(&conn), 1, "repeats push nothing");
        // A different failure kind latches apart.
        let other = notify_wake(
            &conn,
            "wak_1",
            NotifyPolicy::Failure,
            WakeOutcome::Failure { kind: "check-exit" },
            &input("Wake check failed", None),
        )
        .expect("wake");
        assert!(matches!(other, Some(FailureNotice::Notified(_))));
        assert_eq!(outbox_rows(&conn), 2);
        // A success recovers every kind; `failure` sends nothing for it.
        let ok = notify_wake(
            &conn,
            "wak_1",
            NotifyPolicy::Failure,
            WakeOutcome::Success,
            &input("Wake ran", None),
        )
        .expect("wake");
        assert_eq!(ok, None);
        assert!(matches!(
            failed(&conn),
            Some(FailureNotice::Notified(ref claimed)) if matches!(**claimed, Claimed::Created(_))
        ));
        assert_eq!(
            outbox_rows(&conn),
            3,
            "after recovery the failure pushes again"
        );
    }

    #[test]
    fn the_notify_declaration_decides_what_pushes() {
        let conn = db();
        let run = |policy, outcome| {
            notify_wake(&conn, "wak_2", policy, outcome, &input("Wake", None)).expect("wake")
        };
        assert_eq!(
            run(NotifyPolicy::Never, WakeOutcome::Failure { kind: "x" }),
            None
        );
        assert_eq!(run(NotifyPolicy::Never, WakeOutcome::Success), None);
        assert_eq!(run(NotifyPolicy::Failure, WakeOutcome::Success), None);
        assert_eq!(outbox_rows(&conn), 0);
        assert!(matches!(
            run(NotifyPolicy::Always, WakeOutcome::Success),
            Some(FailureNotice::Notified(ref claimed)) if matches!(**claimed, Claimed::Created(_))
        ));
        assert!(matches!(
            run(NotifyPolicy::Always, WakeOutcome::Failure { kind: "x" }),
            Some(FailureNotice::Notified(ref claimed)) if matches!(**claimed, Claimed::Created(_))
        ));
        assert!(matches!(
            run(NotifyPolicy::Always, WakeOutcome::Failure { kind: "x" }),
            Some(FailureNotice::Repeat { .. })
        ));
        assert_eq!(outbox_rows(&conn), 2);
        assert_eq!("failure".parse(), Ok(NotifyPolicy::Failure));
        assert!("sometimes".parse::<NotifyPolicy>().is_err());
    }

    #[test]
    fn the_secret_lint_names_the_field_never_the_text() {
        let secret = format!("ghp_{}", "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8");
        for (title, body, key, field) in [
            (secret.as_str(), "ok", None, "title"),
            ("ok", secret.as_str(), None, "body"),
            ("ok", "ok", Some(secret.as_str()), "dedupeKey"),
        ] {
            let Err(NotifyError::SecretRejected { field: named, .. }) =
                check_notice(title, body, key)
            else {
                panic!("{field} must be refused as a secret");
            };
            assert_eq!(named, field);
            let response = NotifyError::SecretRejected { field, rule: "x" }.into_response();
            assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        }
    }

    #[test]
    fn titles_bodies_and_keys_are_bounded() {
        assert!(check_notice("ok", "ok", Some("wake:wak_1/fail-1.2")).is_ok());
        let long_title = "t".repeat(TITLE_MAX_CHARS + 1);
        let long_body = "b".repeat(BODY_MAX_CHARS + 1);
        let long_key = "k".repeat(DEDUPE_KEY_MAX_CHARS + 1);
        for (title, body, key) in [
            ("", "ok", None),
            ("two\nlines", "ok", None),
            (long_title.as_str(), "ok", None),
            ("ok", "  ", None),
            ("ok", long_body.as_str(), None),
            ("ok", "ok", Some(long_key.as_str())),
            ("ok", "ok", Some("has space")),
        ] {
            assert!(
                matches!(
                    check_notice(title, body, key),
                    Err(NotifyError::BadRequest(_))
                ),
                "{title:?} {body:?} {key:?}"
            );
        }
        let checked = check_notice(" 标题 ", " 正文 ", Some("")).expect("ok");
        assert_eq!(
            checked,
            CheckedNotice {
                title: "标题".into(),
                body: "正文".into(),
                dedupe_key: None
            }
        );
    }

    #[test]
    fn the_push_body_is_one_cut_line() {
        assert_eq!(push_body("a\n\nb  c"), "a b c");
        let long = "字".repeat(PUSH_BODY_CHARS + 5);
        let cut = push_body(&long);
        assert_eq!(cut.chars().count(), PUSH_BODY_CHARS + 1);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn an_ack_is_idempotent_and_audited_once() {
        let conn = db();
        let Claimed::Created(notice) = claim(&conn, &input("Hi", None)).expect("claim") else {
            panic!("created");
        };
        let first = ack(&conn, &notice.notice_id, "dev_phone")
            .expect("ack")
            .expect("row");
        let second = ack(&conn, &notice.notice_id, "dev_desk")
            .expect("ack")
            .expect("row");
        assert!(first.acked_at.is_some());
        assert_eq!(first.acked_at, second.acked_at);
        let audits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM audit_log WHERE action = 'owner.notice.ack'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(audits, 1);
        assert!(
            ack(&conn, "ntc_missing", "dev_phone")
                .expect("ack")
                .is_none()
        );
        let unacked = list(
            &conn,
            &ListFilter {
                unacked: true,
                chapter: None,
                limit: 50,
            },
        )
        .expect("list");
        assert!(unacked.is_empty());
    }
}
