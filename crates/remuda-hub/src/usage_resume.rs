//! Resume after a usage-limit reset (cb-usage-resume; D-060 R3,
//! main-agent.md §11.6).
//!
//! When a child's root turn fails on a usage limit, the Hub learns the reset
//! time, waits for it durably, writes the reset fact to the child's parent at
//! the fire time, and, when the child's `autoContinue` is on, sends it one
//! continuation input.
//!
//! * **The reset time** is the first available of: the harness's structured
//!   rate-limit event in the failed turn ([`ResetSource::Structured`]); a
//!   subscription window of the instance's native-login profile that is
//!   actually full ([`ResetSource::SupplyWindow`], `resetsAt` in
//!   `crate::supply`); a time in the error text that states its time zone
//!   ([`ResetSource::ErrorText`]).
//! * **Only subscription windows wait** ([`classify`]). A 429 from an API key
//!   or a gateway is usually a per-minute rate limit: it creates no row, even
//!   when it carries a retry time, and the turn's usual `turn-failed` fact is
//!   all it produces.
//! * **Every Node is remote.** The Hub cannot tell which Node shares its
//!   machine, so a wait only ever uses a reset time a Node reported: its
//!   journal's structured event, the `resetsAt` of a window whose fill was
//!   reported, or the zoned time in its own error text. The Hub's own
//!   backoff estimate (a cooldown without `resetsAt`) never makes a wait.
//! * **Rows** (`resume_on_reset`, in `hub.sqlite`, so `scripts/hub-backup.sh`
//!   carries them): the instance, the run generation, the supply window, the
//!   fire time (`resetsAt` + 60 s + a jitter in [0, 30 s)), the continuation
//!   count (`attempts`) and the state. The inbox tick claims each row once.
//!   A reset more than 8 days away makes no row.
//! * **The reset fact is always written first** at the fire time, as the
//!   inbox's `supply-reset` kind (cb-inbox-facts), whether or not
//!   `autoContinue` is on; delivery is the inbox's (cb-inbox-deliver).
//! * **The continuation** is a new `instance.send` with origin `hub` and a
//!   commandId persisted with its row (D-055: a new command, never a replay):
//!   `continue` when the failed turn produced output, otherwise that turn's
//!   own input again. It carries the Hub-internal initiator of the subscriber
//!   lineage's live generation; F re-attributes a waiting row to the
//!   successor ([`reattribute`]), and a continuation F cancelled before its
//!   forward goes back to waiting and is formed again under a new id.
//! * **The cap.** At most [`CONTINUATION_CAP`] automatic continuations
//!   between two pieces of substantive progress. *Substantive progress* is a
//!   root turn of the instance that ends after journaling at least one tool
//!   call or one non-empty assistant text that is not the harness's
//!   usage-limit notice. A continuation whose turn hits the limit again
//!   without doing anything is not progress, so a loop stops after three.
//! * **Cancel.** Any input to the instance that the Hub did not write (a
//!   Human's, an Agent's or a Bot's `instance.send`, `instance.respond`,
//!   `instance.keys`, `tty.write` — how a switch-model reaches it — or an
//!   `instance.configure`) cancels its wait in the same transaction, by a
//!   trigger on the command insert; an interaction answer, a direct RPC
//!   that writes no command row, cancels it through [`cancel`]. A
//!   continuation formed and not yet forwarded is cancelled with it.
//! * **Logs** carry categories and counts only: never error text, worker
//!   output or ids.

use crate::config::now_rfc3339;
use crate::store::{StoreError, ensure_column};
use remuda_protocol::rate_limit_event as rle;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;

/// Automatic continuations allowed between two pieces of substantive
/// progress.
pub(crate) const CONTINUATION_CAP: i64 = 3;
/// A reset further away than this makes no row, only the fact.
pub(crate) const MAX_WAIT: time::Duration = time::Duration::days(8);
/// Fixed delay after `resetsAt` before the fire time.
pub(crate) const FIRE_DELAY: time::Duration = time::Duration::seconds(60);
/// The fire time's jitter is drawn from `[0, JITTER_MS)`.
pub(crate) const JITTER_MS: u64 = 30_000;
/// Characters of the harness's limit notice the tracker keeps, enough for
/// its reset clause.
const NOTICE_MAX_CHARS: usize = 300;
/// How long an input hold keeps a wait from forming its continuation when
/// the answer that took it never settles it (a crash between the two
/// phases): after this the hold is ignored.
pub(crate) const INPUT_HOLD: time::Duration = time::Duration::seconds(60);
/// The continuation text when the failed turn produced output.
pub(crate) const CONTINUE_TEXT: &str = "continue";

/// Row states written in Rust; the triggers and queries spell the same
/// words. `waiting` (before the fire time) → `holding` (fired; the
/// continuation waits for the instance and its initiator lineage) →
/// `continued` (the command is formed). A fired row with no continuation is
/// `done` (`reason`: `auto-continue-off`, `not-live`) or `capped`; input
/// to the instance, a newer wait or a deleted instance makes a row
/// `cancelled`.
pub(crate) mod state {
    pub(crate) const HOLDING: &str = "holding";
    pub(crate) const DONE: &str = "done";
    pub(crate) const CAPPED: &str = "capped";
}

/// Idempotent DDL; no `user_version` rung. The tables live in `hub.sqlite`,
/// which `scripts/hub-backup.sh` carries whole. Run after `inbox`,
/// `inbox_deliver` and `commands` exist; the triggers are rebuilt on every
/// open.
pub(crate) fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    ensure_column(
        conn,
        "instances",
        "auto_continue",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS resume_on_reset (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            instance_id TEXT NOT NULL,
            run_generation INTEGER NOT NULL,
            window_id TEXT NOT NULL,
            source TEXT NOT NULL,
            profile_id TEXT,
            resets_at TEXT NOT NULL,
            fire_at TEXT NOT NULL,
            jitter_ms INTEGER NOT NULL,
            attempts INTEGER NOT NULL DEFAULT 0,
            state TEXT NOT NULL,
            reason TEXT,
            turn_command_id TEXT,
            turn_output INTEGER NOT NULL DEFAULT 0,
            command_id TEXT,
            initiator_instance_id TEXT,
            initiator_lineage_id TEXT,
            initiator_generation INTEGER,
            created_at TEXT NOT NULL,
            fired_at TEXT,
            continued_at TEXT,
            updated_at TEXT NOT NULL,
            UNIQUE (instance_id, run_generation)
         );
         CREATE INDEX IF NOT EXISTS resume_on_reset_due
            ON resume_on_reset(fire_at) WHERE state = 'waiting';
         CREATE INDEX IF NOT EXISTS resume_on_reset_open
            ON resume_on_reset(instance_id) WHERE state IN ('waiting', 'holding', 'continued');
         CREATE TABLE IF NOT EXISTS resume_trackers (
            instance_id TEXT PRIMARY KEY,
            tracker_json TEXT NOT NULL,
            updated_at TEXT NOT NULL
         );
         -- §11.6 Cancel: input the Hub did not write cancels the wait, and a
         -- formed continuation not yet forwarded with it. A configure is how
         -- a switch-model reaches the instance.
         DROP TRIGGER IF EXISTS resume_cancel_on_input;
         CREATE TRIGGER resume_cancel_on_input AFTER INSERT ON commands
         WHEN NEW.instance_id IS NOT NULL
          AND NEW.operation IN ('instance.send', 'instance.respond', 'instance.keys',
                                'tty.write', 'instance.configure')
          AND COALESCE(json_extract(NEW.payload_json, '$.origin'), '') <> 'hub'
          AND EXISTS (SELECT 1 FROM resume_on_reset r
                       WHERE r.instance_id = NEW.instance_id
                         AND r.state IN ('waiting', 'holding', 'continued'))
         BEGIN
           UPDATE commands
              SET state = 'settled', resolution = 'clear',
                  settlement_outcome = 'cancelled', settlement_reason = 'resume-cancelled',
                  updated_at = NEW.created_at
            WHERE id IN (SELECT r.command_id FROM resume_on_reset r
                          WHERE r.instance_id = NEW.instance_id AND r.state = 'continued')
              AND state = 'queued' AND forwarded = 0;
           UPDATE resume_on_reset
              SET state = 'cancelled', reason = 'input', updated_at = NEW.created_at
            WHERE instance_id = NEW.instance_id
              AND (state IN ('waiting', 'holding')
                   OR (state = 'continued' AND EXISTS (
                         SELECT 1 FROM commands c
                          WHERE c.id = resume_on_reset.command_id
                            AND c.settlement_reason = 'resume-cancelled')));
         END;
         -- §11.6, F: a continuation that provably never reached the
         -- instance goes back to holding and is formed again, under a new id,
         -- with the initiator F re-attributed it to: F cancelled it before
         -- its forward, the send gate withheld it, the Node refused it as
         -- fenced, or the Node settled it as never dispatched. One the Hub
         -- gave up on (its same-id attempts spent, or the Node no longer
         -- knows the instance) ends the wait. Any other outcome keeps it: at
         -- most one continuation per wait.
         DROP TRIGGER IF EXISTS resume_release_fenced;
         DROP TRIGGER IF EXISTS resume_continuation_settled;
         CREATE TRIGGER resume_continuation_settled AFTER UPDATE OF state ON commands
         WHEN NEW.state = 'settled' AND OLD.state <> 'settled'
          AND EXISTS (SELECT 1 FROM resume_on_reset
                       WHERE command_id = NEW.id AND state = 'continued')
         BEGIN
           UPDATE resume_on_reset
              SET state = 'holding', command_id = NULL, attempts = 0,
                  continued_at = NULL, reason = 'released', updated_at = NEW.updated_at
            WHERE command_id = NEW.id AND state = 'continued'
              AND (NEW.settlement_reason = 'fenced'
                   OR NEW.settlement_outcome = 'withheld'
                   OR NEW.settlement_not_dispatched = 1);
           UPDATE resume_on_reset
              SET state = 'done', reason = NEW.settlement_reason, updated_at = NEW.updated_at
            WHERE command_id = NEW.id AND state = 'continued'
              AND NEW.settlement_reason IN ('not-delivered', 'instance-not-found');
         END;
         -- A deleted instance takes no continuation.
         DROP TRIGGER IF EXISTS resume_instance_deleted;
         CREATE TRIGGER resume_instance_deleted AFTER DELETE ON instances
         BEGIN
           DELETE FROM resume_trackers WHERE instance_id = OLD.id;
           UPDATE resume_on_reset SET state = 'cancelled', reason = 'deleted'
            WHERE instance_id = OLD.id AND state IN ('waiting', 'holding');
         END;",
    )?;
    // r3: an answer being committed holds the wait (`hold_for_input`).
    ensure_column(conn, "resume_on_reset", "input_hold_at", "TEXT")?;
    Ok(())
}

// ── reset time and classification ───────────────────────────────────────────

/// Where a reset time came from, in order of preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResetSource {
    /// The harness's structured rate-limit event (`rate_limit_event`).
    Structured,
    /// A full subscription window of the instance's native-login profile.
    SupplyWindow,
    /// A time in the error text that states its zone.
    ErrorText,
}

impl ResetSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Structured => "structured",
            Self::SupplyWindow => "supply-window",
            Self::ErrorText => "error-text",
        }
    }
}

/// The credential an instance's harness runs on, as far as the Hub knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Credential {
    /// A native-login profile (`kind: native`): a subscription account.
    Subscription,
    /// A `gateway` or `direct` profile: an API key or a gateway relay.
    ApiKeyOrGateway,
    /// No real profile: the harness's own login on the host.
    HostLogin,
}

/// What a failed turn's evidence shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Evidence {
    /// The harness reported a subscription window rejected the turn.
    WindowRejected {
        window: String,
        resets_at: Option<i64>,
    },
    /// The harness's subscription usage-limit notice ("You've hit your
    /// limit", "session limit", "weekly limit", "Opus limit", …).
    SubscriptionNotice,
    /// An API rate limit: `rate_limit_error`, `API Error: 429`, a bare 429.
    ApiRateLimit,
    /// Not a usage limit at all.
    None,
}

/// The classification of one failed turn (§11.6 "Only subscription windows
/// wait"): the table, explicitly.
///
/// | credential          | evidence                        | class        |
/// |---------------------|---------------------------------|--------------|
/// | api key or gateway  | any                             | api-key-or-gateway: no wait |
/// | subscription / host login | window rejected or subscription notice | subscription: wait |
/// | subscription / host login | API rate limit only        | api-rate-limit: no wait |
/// | any                 | none                            | not a limit  |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    Subscription,
    ApiKeyOrGateway,
    ApiRateLimit,
    NotALimit,
}

impl Class {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Subscription => "subscription",
            Self::ApiKeyOrGateway => "api-key-or-gateway",
            Self::ApiRateLimit => "api-rate-limit",
            Self::NotALimit => "not-a-limit",
        }
    }
}

#[must_use]
pub(crate) fn classify(credential: Credential, evidence: &Evidence) -> Class {
    match (credential, evidence) {
        (_, Evidence::None) => Class::NotALimit,
        (Credential::ApiKeyOrGateway, _) => Class::ApiKeyOrGateway,
        (_, Evidence::WindowRejected { .. } | Evidence::SubscriptionNotice) => Class::Subscription,
        (_, Evidence::ApiRateLimit) => Class::ApiRateLimit,
    }
}

/// The harness's subscription usage-limit notice.
#[must_use]
pub(crate) fn is_subscription_notice(text: &str) -> bool {
    let text = text.to_ascii_lowercase().replace('\u{2019}', "'");
    const PHRASES: &[&str] = &[
        "hit your limit",
        "hit your session limit",
        "hit your weekly limit",
        "hit your opus limit",
        "hit your sonnet limit",
        "hit your usage limit",
        "usage limit reached",
        "5-hour limit reached",
        "session limit reached",
        "weekly limit reached",
        "opus limit reached",
    ];
    PHRASES.iter().any(|phrase| text.contains(phrase))
}

/// An API rate limit's text (per-minute limits; never a wait).
#[must_use]
pub(crate) fn is_api_rate_limit(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("rate_limit_error")
        || text.contains("api error: 429")
        || text.contains("request rejected (429)")
        || text.contains("too many requests")
        || (text.contains("429") && (text.contains("rate") || text.contains("limit")))
}

/// A reset time stated in `text` with its time zone, resolved to the first
/// such instant after `observed`. `None` when the text states no zone: a
/// wall-clock time whose zone the Hub would have to guess is never used.
///
/// Recognised: `resets 3pm (Asia/Shanghai)`, `resets 2:30pm (UTC)`,
/// `resets Nov 5, 9am (America/New_York)`, `resets at 15:00 UTC+8`, and an
/// RFC 3339 time with an offset after `reset`.
#[must_use]
pub(crate) fn reset_in_text(text: &str, observed: OffsetDateTime) -> Option<OffsetDateTime> {
    let lower = text.to_ascii_lowercase();
    let at = lower.find("reset")?;
    let rest = &text[at..];
    let rest = rest
        .char_indices()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, _)| rest[i..].trim_start())?;
    if let Some(instant) = rest
        .split_whitespace()
        .next()
        .map(|word| word.trim_end_matches([')', '.', ',']))
        .and_then(|word| {
            OffsetDateTime::parse(word, &time::format_description::well_known::Rfc3339).ok()
        })
    {
        return Some(instant);
    }
    let mut words = Tokens::new(rest);
    if words
        .peek_lower()
        .as_deref()
        .is_some_and(|w| w == "at" || w == "on")
    {
        words.next();
    }
    // Optional date: `Nov 5` (`,` and `at` may follow).
    let mut date = None;
    if let Some(month) = words.peek_lower().as_deref().and_then(month_of) {
        words.next();
        let day: u8 = words.next()?.trim_end_matches(',').parse().ok()?;
        date = Some((month, day));
        if words.peek_lower().as_deref() == Some("at") {
            words.next();
        }
    }
    let (hour, minute) = parse_clock(&words.next()?)?;
    let zone = zone_in(words.rest())?;
    resolve(observed, date, hour, minute, &zone)
}

/// The words of a reset clause, `·`/`|` and separators aside.
struct Tokens<'a> {
    words: std::iter::Peekable<std::str::SplitWhitespace<'a>>,
    rest: &'a str,
}

impl<'a> Tokens<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            words: text.split_whitespace().peekable(),
            rest: text,
        }
    }

    fn peek_lower(&mut self) -> Option<String> {
        self.words.peek().map(|word| word.to_ascii_lowercase())
    }

    fn next(&mut self) -> Option<String> {
        let word = self.words.next()?;
        if let Some(at) = self.rest.find(word) {
            self.rest = &self.rest[at + word.len()..];
        }
        Some(word.to_owned())
    }

    fn rest(&self) -> &'a str {
        self.rest.trim_start()
    }
}

fn month_of(word: &str) -> Option<time::Month> {
    use time::Month::*;
    let word = word.trim_end_matches([',', '.']);
    let months = [
        ("jan", January),
        ("feb", February),
        ("mar", March),
        ("apr", April),
        ("may", May),
        ("jun", June),
        ("jul", July),
        ("aug", August),
        ("sep", September),
        ("oct", October),
        ("nov", November),
        ("dec", December),
    ];
    if word.len() < 3 {
        return None;
    }
    months
        .iter()
        .find(|(prefix, _)| word.starts_with(prefix))
        .map(|(_, month)| *month)
}

/// `3pm`, `3:30pm`, `11am`, `15:00`.
fn parse_clock(word: &str) -> Option<(u8, u8)> {
    let word = word.trim_end_matches([',', '.']).to_ascii_lowercase();
    let (body, meridiem) = if let Some(body) = word.strip_suffix("am") {
        (body, Some(false))
    } else if let Some(body) = word.strip_suffix("pm") {
        (body, Some(true))
    } else {
        (word.as_str(), None)
    };
    let (hour, minute) = match body.split_once(':') {
        Some((hour, minute)) => (hour.parse::<u8>().ok()?, minute.parse::<u8>().ok()?),
        None if meridiem.is_some() => (body.parse::<u8>().ok()?, 0),
        None => return None,
    };
    if minute > 59 {
        return None;
    }
    let hour = match meridiem {
        Some(pm) if (1..=12).contains(&hour) => (hour % 12) + if pm { 12 } else { 0 },
        Some(_) => return None,
        None if hour <= 23 => hour,
        None => return None,
    };
    Some((hour, minute))
}

/// A stated zone right after the clock: `(Asia/Shanghai)`, `(UTC)`, `UTC`,
/// `GMT+8`, `UTC-05:00`, `+08:00`.
fn zone_in(rest: &str) -> Option<jiff::tz::TimeZone> {
    let rest = rest.trim_start();
    let token = if let Some(inner) = rest.strip_prefix('(') {
        inner.split(')').next()?.trim()
    } else {
        rest.split_whitespace()
            .next()?
            .trim_end_matches([')', '.', ','])
    };
    if token.is_empty() {
        return None;
    }
    let upper = token.to_ascii_uppercase();
    for prefix in ["UTC", "GMT"] {
        if let Some(offset) = upper.strip_prefix(prefix) {
            if offset.is_empty() {
                return Some(jiff::tz::TimeZone::UTC);
            }
            return fixed_offset(offset);
        }
    }
    if token.starts_with(['+', '-']) {
        return fixed_offset(token);
    }
    if token.contains('/') {
        return jiff::tz::TimeZone::get(token).ok();
    }
    None
}

/// `+8`, `-05:00`, `+0530`.
fn fixed_offset(raw: &str) -> Option<jiff::tz::TimeZone> {
    if !raw.is_ascii() {
        return None;
    }
    let (sign, digits) = match raw.as_bytes().first()? {
        b'+' => (1_i32, &raw[1..]),
        b'-' => (-1_i32, &raw[1..]),
        _ => return None,
    };
    let (hours, minutes) = match digits.split_once(':') {
        Some((h, m)) => (h.parse::<i32>().ok()?, m.parse::<i32>().ok()?),
        None if digits.len() == 4 => (digits[..2].parse().ok()?, digits[2..].parse().ok()?),
        None => (digits.parse().ok()?, 0),
    };
    if !(0..=14).contains(&hours) || !(0..=59).contains(&minutes) {
        return None;
    }
    let seconds = sign * (hours * 3600 + minutes * 60);
    jiff::tz::Offset::from_seconds(seconds)
        .ok()
        .map(jiff::tz::TimeZone::fixed)
}

/// The first `hour:minute` (on `date`, when given) in `zone` after
/// `observed`.
fn resolve(
    observed: OffsetDateTime,
    date: Option<(time::Month, u8)>,
    hour: u8,
    minute: u8,
    zone: &jiff::tz::TimeZone,
) -> Option<OffsetDateTime> {
    let now = jiff::Timestamp::from_second(observed.unix_timestamp()).ok()?;
    let local = now.to_zoned(zone.clone());
    let at = |year: i16, month: i8, day: i8| -> Option<i64> {
        jiff::civil::Date::new(year, month, day)
            .ok()?
            .at(i8::try_from(hour).ok()?, i8::try_from(minute).ok()?, 0, 0)
            .to_zoned(zone.clone())
            .ok()
            .map(|zoned| zoned.timestamp().as_second())
    };
    let candidate = match date {
        Some((month, day)) => {
            let month = i8::try_from(u8::from(month)).ok()?;
            let day = i8::try_from(day).ok()?;
            let this_year = at(local.year(), month, day)?;
            // The next occurrence: a date and time already past this year,
            // earlier today included, is next year's.
            if this_year <= now.as_second() {
                at(local.year().checked_add(1)?, month, day)?
            } else {
                this_year
            }
        }
        None => {
            let today = at(local.year(), local.month(), local.day())?;
            if today > now.as_second() {
                today
            } else {
                let tomorrow = local.date().tomorrow().ok()?;
                at(tomorrow.year(), tomorrow.month(), tomorrow.day())?
            }
        }
    };
    OffsetDateTime::from_unix_timestamp(candidate).ok()
}

// ── the per-instance tracker ────────────────────────────────────────────────

/// Journal-derived state the wait needs, folded in the append transaction.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Tracker {
    /// The current root turn journaled a tool call or a non-notice
    /// assistant text.
    #[serde(default)]
    turn_output: bool,
    /// The latest structured rejection in the current root turn:
    /// `(window, resetsAt epoch seconds)`.
    #[serde(default)]
    rejected: Option<(String, Option<i64>)>,
    /// The harness's limit notice in the current root turn, bounded.
    #[serde(default)]
    notice: Option<String>,
    /// The newest wait row of the instance when it last made substantive
    /// progress (a root turn that ended with output): the cap counts the
    /// continuations of later rows. A row id, not a time, so an injected
    /// tick clock and the journal's clock cannot disagree.
    #[serde(default)]
    progress_mark: Option<i64>,
    /// The classification of the latest failed root turn, for its
    /// `turn-failed` fact: `(run, detail)`.
    #[serde(default)]
    verdict: Option<(i64, Value)>,
    /// The harness's credential source from its session start (Claude
    /// `apiKeySource`): `none` is a claude.ai login.
    #[serde(default)]
    api_key_source: Option<String>,
}

fn load_tracker(conn: &Connection, instance_id: &str) -> Result<Tracker, StoreError> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT tracker_json FROM resume_trackers WHERE instance_id = ?1",
            params![instance_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(raw
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default())
}

fn save_tracker(conn: &Connection, instance_id: &str, tracker: &Tracker) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO resume_trackers (instance_id, tracker_json, updated_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(instance_id) DO UPDATE SET
            tracker_json = excluded.tracker_json, updated_at = excluded.updated_at",
        params![instance_id, serde_json::to_string(tracker)?, now_rfc3339()],
    )?;
    Ok(())
}

fn text_at<'a>(payload: &'a Value, pointer: &str) -> Option<&'a str> {
    payload.pointer(pointer).and_then(Value::as_str)
}

fn knowledge(value: Option<&Value>) -> Option<&str> {
    let value = value?;
    value
        .as_str()
        .or_else(|| value.get("value").and_then(Value::as_str))
}

fn blocks_text(payload: &Value) -> String {
    payload
        .get("blocks")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn bounded(text: &str) -> String {
    text.trim().chars().take(NOTICE_MAX_CHARS).collect()
}

/// The test seam's fixed jitter, when armed.
#[cfg(any(test, feature = "test-faults"))]
pub(crate) mod test_seams {
    use std::sync::Mutex;

    static JITTER: Mutex<Option<u64>> = Mutex::new(None);

    /// Every fire time from now on uses `jitter_ms` (clamped below
    /// [`super::JITTER_MS`]); `None` restores the random draw.
    pub(crate) fn set_jitter(jitter_ms: Option<u64>) {
        *JITTER.lock().unwrap_or_else(|poison| poison.into_inner()) =
            jitter_ms.map(|ms| ms.min(super::JITTER_MS - 1));
    }

    pub(crate) fn jitter() -> Option<u64> {
        *JITTER.lock().unwrap_or_else(|poison| poison.into_inner())
    }
}

/// A jitter in `[0, JITTER_MS)`: random, unless a test injected one.
fn jitter_ms() -> u64 {
    #[cfg(any(test, feature = "test-faults"))]
    if let Some(fixed) = test_seams::jitter() {
        return fixed;
    }
    let bits = uuid::Uuid::new_v4().as_u128();
    u64::try_from(bits % u128::from(JITTER_MS)).unwrap_or(0)
}

/// The instance's credential: its provider profile when it has a real one;
/// else the harness's own report (`apiKeySource`, `none` being a claude.ai
/// login); else its host's inventory (a Claude configured for a gateway).
fn credential_of(
    conn: &Connection,
    instance_id: &str,
    api_key_source: Option<&str>,
) -> Result<(Credential, Option<String>), StoreError> {
    let profile: Option<String> = conn
        .query_row(
            "SELECT json_extract(spec_json, '$.providerProfileId') FROM instances WHERE id = ?1",
            params![instance_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let Some(profile) = profile.filter(|id| crate::provider_resolve::is_real_profile_id(id)) else {
        if let Some(source) = api_key_source {
            return Ok((
                if source == "none" {
                    Credential::HostLogin
                } else {
                    Credential::ApiKeyOrGateway
                },
                None,
            ));
        }
        let cli: Option<String> = conn
            .query_row(
                "SELECT h.cli_json FROM hosts h JOIN instances i ON i.host_id = h.id
                  WHERE i.id = ?1",
                params![instance_id],
                |row| row.get(0),
            )
            .optional()?;
        let gateway = cli
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .and_then(|cli| cli.as_array().cloned())
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item.get("kind").and_then(Value::as_str) == Some("claude")
                        && (item.get("auth").and_then(Value::as_str) == Some("gateway-native")
                            || item.get("nativeGateway").and_then(Value::as_bool) == Some(true))
                })
            });
        return Ok((
            if gateway {
                Credential::ApiKeyOrGateway
            } else {
                Credential::HostLogin
            },
            None,
        ));
    };
    let kind: Option<String> = conn
        .query_row(
            "SELECT kind FROM provider_profiles WHERE id = ?1",
            params![profile],
            |row| row.get(0),
        )
        .optional()?;
    Ok(match kind.as_deref() {
        Some("native") => (Credential::Subscription, Some(profile)),
        // A gateway, a direct key, or a profile the Hub no longer knows: a
        // profile-routed instance never runs on a subscription login.
        _ => (Credential::ApiKeyOrGateway, Some(profile)),
    })
}

/// Source 2: a full window of the native-login profile that covers the
/// instance's model family, with a reported `resetsAt` still ahead. The
/// Hub's backoff `cooldownUntil` alone never counts.
fn full_window(
    conn: &Connection,
    instance_id: &str,
    profile_id: &str,
    now: i64,
) -> Result<Option<(String, i64)>, StoreError> {
    let row: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT p.supply_json, json_extract(i.spec_json, '$.model')
               FROM provider_profiles p, instances i
              WHERE p.id = ?1 AND i.id = ?2",
            params![profile_id, instance_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((supply, model)) = row else {
        return Ok(None);
    };
    let Ok(supply) = serde_json::from_str::<remuda_protocol::SupplyProfile>(&supply) else {
        return Ok(None);
    };
    let model = model.unwrap_or_default();
    let family =
        crate::model_catalog::lookup(&model).map_or(model.clone(), |row| row.family.to_string());
    Ok(supply
        .windows
        .iter()
        .filter(|window| window.applies_to_family(&family) || window.applies_to_family(&model))
        .filter(|window| window.used_percent.is_some_and(|pct| pct >= 100.0))
        .filter_map(|window| window.resets_at.map(|at| (window.id.clone(), at)))
        .filter(|(_, at)| *at > now)
        .max_by_key(|(_, at)| *at))
}

/// What a failed root turn of `instance_id` implies, decided in its append.
struct Verdict {
    class: Class,
    wait: Option<Wait>,
    /// Why a subscription limit makes no row: `no-reset-time`,
    /// `beyond-8-days`.
    no_wait: Option<&'static str>,
    resets_at: Option<OffsetDateTime>,
    window: Option<String>,
    source: Option<ResetSource>,
}

struct Wait {
    resets_at: OffsetDateTime,
    window: String,
    source: ResetSource,
    profile_id: Option<String>,
}

fn decide(
    conn: &Connection,
    instance_id: &str,
    tracker: &Tracker,
    last_error: Option<&str>,
    observed: OffsetDateTime,
) -> Result<Verdict, StoreError> {
    let (credential, profile_id) =
        credential_of(conn, instance_id, tracker.api_key_source.as_deref())?;
    let texts: Vec<&str> = [last_error, tracker.notice.as_deref()]
        .into_iter()
        .flatten()
        .collect();
    let evidence = if let Some((window, resets_at)) = &tracker.rejected {
        Evidence::WindowRejected {
            window: window.clone(),
            resets_at: *resets_at,
        }
    } else if texts.iter().any(|text| is_subscription_notice(text)) {
        Evidence::SubscriptionNotice
    } else if texts.iter().any(|text| is_api_rate_limit(text)) {
        Evidence::ApiRateLimit
    } else {
        Evidence::None
    };
    let class = classify(credential, &evidence);
    let mut verdict = Verdict {
        class,
        wait: None,
        no_wait: None,
        resets_at: None,
        window: None,
        source: None,
    };
    if class != Class::Subscription {
        return Ok(verdict);
    }
    let now = observed.unix_timestamp();
    // 1. The structured event.
    let mut found = match &evidence {
        Evidence::WindowRejected {
            window,
            resets_at: Some(at),
        } => OffsetDateTime::from_unix_timestamp(*at)
            .ok()
            .map(|at| (window.clone(), at, ResetSource::Structured)),
        _ => None,
    };
    // 2. A full subscription window of the native-login profile.
    if found.is_none()
        && credential == Credential::Subscription
        && let Some(profile) = profile_id.as_deref()
        && let Some((window, at)) = full_window(conn, instance_id, profile, now)?
    {
        found = OffsetDateTime::from_unix_timestamp(at)
            .ok()
            .map(|at| (window, at, ResetSource::SupplyWindow));
    }
    // 3. A time in the error text, only when it states its zone.
    if found.is_none() {
        let window = match &evidence {
            Evidence::WindowRejected { window, .. } => window.clone(),
            _ => "subscription".to_owned(),
        };
        found = texts
            .iter()
            .find_map(|text| reset_in_text(text, observed))
            .map(|at| (window, at, ResetSource::ErrorText));
    }
    let Some((window, resets_at, source)) = found else {
        verdict.no_wait = Some("no-reset-time");
        return Ok(verdict);
    };
    verdict.resets_at = Some(resets_at);
    verdict.window = Some(window.clone());
    verdict.source = Some(source);
    // A reset already behind the turn's end is not a wait: it would fire at
    // once, on a window that may already have reset and filled again.
    if resets_at <= observed {
        verdict.no_wait = Some("reset-passed");
        return Ok(verdict);
    }
    if resets_at - observed > MAX_WAIT {
        verdict.no_wait = Some("beyond-8-days");
        return Ok(verdict);
    }
    verdict.wait = Some(Wait {
        resets_at,
        window,
        source,
        profile_id: profile_id.filter(|_| credential == Credential::Subscription),
    });
    Ok(verdict)
}

/// The Hub-internal initiator for input the Hub writes to `instance_id` on
/// its subscriber's behalf: the parent lineage's live chapter and generation;
/// for an instance with no parent lineage, its own continuity lineage's; else
/// none.
fn initiator_for(
    conn: &Connection,
    instance_id: &str,
) -> Result<Option<(String, String, i64)>, StoreError> {
    let lineage = match crate::inbox::subscriber_of(conn, instance_id)? {
        Some(parent) => parent,
        None => {
            let own = crate::store::lineage_id_of(conn, instance_id)?;
            match own {
                Some(own)
                    if conn.query_row(
                        "SELECT EXISTS (SELECT 1 FROM lineages WHERE lineage_id = ?1)",
                        params![own],
                        |row| row.get::<_, bool>(0),
                    )? =>
                {
                    own
                }
                _ => return Ok(None),
            }
        }
    };
    live_initiator(conn, &lineage)
}

/// `(chapter, lineage, generation)` of `lineage_id`'s live generation: a
/// continuity lineage's current chapter, or the one instance of a lineage
/// without continuity.
fn live_initiator(
    conn: &Connection,
    lineage_id: &str,
) -> Result<Option<(String, String, i64)>, StoreError> {
    let continuity: Option<(String, i64)> = conn
        .query_row(
            "SELECT current_instance_id, generation FROM lineages WHERE lineage_id = ?1",
            params![lineage_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((current, generation)) = continuity {
        return Ok(Some((current, lineage_id.to_owned(), generation)));
    }
    let generation: Option<i64> = conn
        .query_row(
            "SELECT generation FROM instances WHERE id = ?1",
            params![lineage_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(generation.map(|generation| (lineage_id.to_owned(), lineage_id.to_owned(), generation)))
}

/// Whether `event` is a root (not subagent) turn lifecycle named `name`.
fn root_turn_named(payload: &Value, name: &str) -> bool {
    text_at(payload, "/type") == Some("native")
        && text_at(payload, "/topic") == Some("turn")
        && text_at(payload, "/nativeName") == Some(name)
        && !crate::store::native_payload_is_subagent(payload)
}

/// Fold one appended (not replayed) journal event, in its append
/// transaction, before the inbox fold (which reads the verdict for the
/// failed turn's fact). `turn_ended` is the projection's busy→idle verdict.
pub(crate) fn fold(
    conn: &Connection,
    instance_id: &str,
    event: &Value,
    hub_now: &str,
    turn_ended: bool,
) -> Result<(), StoreError> {
    let kind = event.get("kind").and_then(Value::as_str).unwrap_or("");
    let payload = event.get("payload").unwrap_or(&Value::Null);
    let rate_limit = kind == "lifecycle"
        && text_at(payload, "/type") == Some("native")
        && text_at(payload, "/nativeName") == Some(rle::NATIVE_NAME);
    let started = kind == "lifecycle" && root_turn_named(payload, "turn_started");
    let result = kind == "lifecycle" && root_turn_named(payload, "result");
    let assistant =
        kind == "message" && payload.get("role").and_then(Value::as_str) == Some("assistant");
    let tool = kind == "tool_call";
    let key_source = (kind == "lifecycle" && text_at(payload, "/topic") == Some("session"))
        .then(|| {
            rle::api_key_source(text_at(
                payload,
                &format!("/relatedIds/{}", rle::API_KEY_SOURCE),
            ))
        })
        .flatten();
    if !(rate_limit || started || result || assistant || tool || turn_ended || key_source.is_some())
    {
        return Ok(());
    }
    let mut tracker = load_tracker(conn, instance_id)?;
    let original = tracker.clone();
    if key_source.is_some() {
        tracker.api_key_source = key_source;
    }
    if started {
        tracker.turn_output = false;
        tracker.rejected = None;
        tracker.notice = None;
    }
    if rate_limit && text_at(payload, &format!("/relatedIds/{}", rle::STATUS)) == Some("rejected") {
        let window = text_at(payload, &format!("/relatedIds/{}", rle::WINDOW))
            .unwrap_or("subscription")
            .to_owned();
        let resets_at = text_at(payload, &format!("/relatedIds/{}", rle::RESETS_AT))
            .and_then(|raw| raw.parse::<i64>().ok());
        tracker.rejected = Some((window, resets_at));
    }
    if assistant {
        let text = blocks_text(payload);
        if is_subscription_notice(&text) {
            tracker.notice = Some(bounded(&text));
        } else if !text.trim().is_empty() {
            tracker.turn_output = true;
        }
    }
    if tool {
        tracker.turn_output = true;
    }
    let structured_end = result
        && (text_at(payload, "/relatedIds/completedTurn").is_some()
            || text_at(payload, "/relatedIds/settledRootTurn") == Some("true"));
    // A carrier without turn boundaries ends a turn on its busy→idle edge;
    // a structured carrier's turn ends only at its root result.
    let unstructured_end =
        turn_ended && !result && !crate::inbox::structured_carrier(conn, instance_id)?;
    if structured_end || unstructured_end {
        let failed = knowledge(payload.get("status")) == Some("error")
            || text_at(payload, "/relatedIds/outcome")
                .is_some_and(|o| o.eq_ignore_ascii_case("failed"));
        let limited = tracker.rejected.is_some() || tracker.notice.is_some();
        if tracker.turn_output {
            tracker.progress_mark = Some(conn.query_row(
                "SELECT COALESCE(MAX(id), 0) FROM resume_on_reset WHERE instance_id = ?1",
                params![instance_id],
                |row| row.get(0),
            )?);
        }
        if failed || (unstructured_end && limited) {
            let observed = crate::inbox::parse_at(hub_now).unwrap_or_else(OffsetDateTime::now_utc);
            let completed = text_at(payload, "/relatedIds/completedTurn");
            let run = crate::inbox::turn_run(conn, instance_id, completed)?;
            let verdict = decide(
                conn,
                instance_id,
                &tracker,
                text_at(payload, "/relatedIds/lastError"),
                observed,
            )?;
            let turn_command = completed.map(str::to_owned).or_else(|| {
                crate::root_turn::derive(conn, instance_id)
                    .ok()
                    .flatten()
                    .map(|turn| turn.command_id)
            });
            if let Some(detail) = record_wait(
                conn,
                instance_id,
                run,
                &tracker,
                &verdict,
                turn_command,
                observed,
            )? {
                tracker.verdict = Some((run, detail));
            }
        }
        tracker.turn_output = false;
        tracker.rejected = None;
        tracker.notice = None;
    }
    if tracker != original {
        save_tracker(conn, instance_id, &tracker)?;
    }
    Ok(())
}

/// Write the wait row a verdict asks for; return the `usageLimit` detail
/// the failed turn's fact carries (`None` when it was not a limit).
fn record_wait(
    conn: &Connection,
    instance_id: &str,
    run: i64,
    tracker: &Tracker,
    verdict: &Verdict,
    turn_command: Option<String>,
    observed: OffsetDateTime,
) -> Result<Option<Value>, StoreError> {
    // D-062 r3 item 5: a claimed herdr pane gets no resume row; its
    // continuation would be Hub voice typed into a process Remuda did not
    // start.
    if crate::herdr_claims::claimed_row(conn, instance_id)? {
        return Ok(None);
    }
    let at = |at: OffsetDateTime| crate::config::rfc3339(at);
    let mut detail = json!({ "class": verdict.class.as_str() });
    if let Some(resets_at) = verdict.resets_at {
        detail["resetsAt"] = json!(at(resets_at));
    }
    if let Some(window) = &verdict.window {
        detail["window"] = json!(window);
    }
    if let Some(source) = verdict.source {
        detail["source"] = json!(source.as_str());
    }
    let outcome = match (&verdict.wait, verdict.class) {
        (_, Class::NotALimit) => {
            tracing::debug!(class = verdict.class.as_str(), "usage limit classified");
            return Ok(None);
        }
        (None, _) => {
            detail["waits"] = json!(false);
            if let Some(reason) = verdict.no_wait {
                detail["reason"] = json!(reason);
            }
            verdict.no_wait.unwrap_or("not-a-subscription-window")
        }
        (Some(wait), _) => {
            // A second end for the same run (a deferred settle correcting
            // its result) finds the row it already wrote: it keeps it, and
            // its fact reports where that wait stands.
            let existing: Option<(String, String, String, String, String)> = conn
                .query_row(
                    "SELECT state, resets_at, fire_at, source, window_id FROM resume_on_reset
                      WHERE instance_id = ?1 AND run_generation = ?2",
                    params![instance_id, run],
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
            if let Some((state, resets_at, fire_at, source, window)) = existing {
                // The wait still stands while it is waiting, holding, or has
                // continued (its continuation is the wait acting); only an
                // ended row (done, capped, cancelled) no longer waits. The
                // row's own state is reported either way (r3 item 4).
                let waits = matches!(state.as_str(), "waiting" | "holding" | "continued");
                detail["resetsAt"] = json!(resets_at);
                detail["fireAt"] = json!(fire_at);
                detail["source"] = json!(source);
                detail["window"] = json!(window);
                detail["waits"] = json!(waits);
                detail["waitState"] = json!(state);
                if !waits {
                    detail["reason"] = json!(state);
                }
                return Ok(Some(detail));
            }
            let jitter = jitter_ms();
            let jitter_i64 = i64::try_from(jitter).unwrap_or(0);
            let fire_at = wait.resets_at + FIRE_DELAY + time::Duration::milliseconds(jitter_i64);
            let initiator = initiator_for(conn, instance_id)?;
            let now = at(observed);
            // A newer wait supersedes an older one still open.
            conn.execute(
                "UPDATE resume_on_reset SET state = 'cancelled', reason = 'superseded', updated_at = ?2
                  WHERE instance_id = ?1 AND state IN ('waiting', 'holding')",
                params![instance_id, now],
            )?;
            let auto: bool = conn
                .query_row(
                    "SELECT auto_continue FROM instances WHERE id = ?1",
                    params![instance_id],
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(false);
            conn.execute(
                "INSERT INTO resume_on_reset
                    (instance_id, run_generation, window_id, source, profile_id, resets_at, fire_at,
                     jitter_ms, attempts, state, turn_command_id, turn_output,
                     initiator_instance_id, initiator_lineage_id, initiator_generation,
                     created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, 'waiting', ?9, ?10, ?11, ?12, ?13, ?14, ?14)
                 ON CONFLICT (instance_id, run_generation) DO NOTHING",
                params![
                    instance_id,
                    run,
                    wait.window,
                    wait.source.as_str(),
                    wait.profile_id,
                    at(wait.resets_at),
                    at(fire_at),
                    jitter_i64,
                    turn_command,
                    tracker.turn_output,
                    initiator.as_ref().map(|i| i.0.clone()),
                    initiator.as_ref().map(|i| i.1.clone()),
                    initiator.as_ref().map(|i| i.2),
                    now,
                ],
            )?;
            // The wait's reset fact replaces the cooling park's (no second
            // `supply-reset` fact for the same limit).
            conn.execute(
                "DELETE FROM inbox_supply_parks WHERE instance_id = ?1",
                params![instance_id],
            )?;
            detail["waits"] = json!(true);
            detail["fireAt"] = json!(at(fire_at));
            detail["autoContinue"] = json!(auto);
            "row"
        }
    };
    tracing::info!(
        class = verdict.class.as_str(),
        source = verdict.source.map_or("none", ResetSource::as_str),
        outcome,
        "usage limit classified"
    );
    Ok(Some(detail))
}

/// The `usageLimit` detail of `instance_id`'s failed run `run`, for its
/// `turn-failed` fact.
pub(crate) fn limit_detail(
    conn: &Connection,
    instance_id: &str,
    run: i64,
) -> Result<Option<Value>, StoreError> {
    Ok(load_tracker(conn, instance_id)?
        .verdict
        .filter(|(verdict_run, _)| *verdict_run == run)
        .map(|(_, detail)| detail))
}

/// Whether `instance_id` has a wait open (a cooling park then records no
/// second reset fact).
pub(crate) fn waits(conn: &Connection, instance_id: &str) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM resume_on_reset
                         WHERE instance_id = ?1 AND state IN ('waiting', 'holding'))",
        params![instance_id],
        |row| row.get(0),
    )?)
}

// ── the tick ────────────────────────────────────────────────────────────────

struct Row {
    id: i64,
    instance_id: String,
    run: i64,
    window: String,
    source: String,
    profile_id: Option<String>,
    resets_at: String,
    fire_at: String,
    turn_command_id: Option<String>,
    turn_output: bool,
    initiator: Option<(String, String, i64)>,
}

fn rows(conn: &Connection, sql: &str, now: &str) -> Result<Vec<Row>, StoreError> {
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt
        .query_map(params![now], |row| {
            let initiator = match (
                row.get::<_, Option<String>>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, Option<i64>>(12)?,
            ) {
                (Some(chapter), Some(lineage), Some(generation)) => {
                    Some((chapter, lineage, generation))
                }
                _ => None,
            };
            Ok(Row {
                id: row.get(0)?,
                instance_id: row.get(1)?,
                run: row.get(2)?,
                window: row.get(3)?,
                source: row.get(4)?,
                profile_id: row.get(5)?,
                resets_at: row.get(6)?,
                fire_at: row.get(7)?,
                turn_command_id: row.get(8)?,
                turn_output: row.get(9)?,
                initiator,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

const ROW_COLUMNS: &str =
    "id, instance_id, run_generation, window_id, source, profile_id, resets_at,
     fire_at, turn_command_id, turn_output, initiator_instance_id, initiator_lineage_id,
     initiator_generation";

/// What one tick did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Due {
    /// Reset facts recorded.
    pub(crate) facts: usize,
    /// Continuations formed: the delivery driver has a forward to make.
    pub(crate) continued: usize,
}

/// One evaluation at `now`, inside the inbox tick's writer transaction:
/// every waiting row whose fire time has come is claimed once (its reset
/// fact, then its continuation or the reason it has none), and every
/// holding row tries its continuation again.
pub(crate) fn due(conn: &Connection, now: OffsetDateTime) -> Result<Due, StoreError> {
    let now_at = crate::config::rfc3339(now);
    let mut out = Due::default();
    let fired = rows(
        conn,
        &format!(
            "SELECT {ROW_COLUMNS} FROM resume_on_reset
              WHERE state = 'waiting' AND fire_at <= ?1 ORDER BY fire_at, id"
        ),
        &now_at,
    )?;
    for row in fired {
        let auto: bool = conn
            .query_row(
                "SELECT auto_continue FROM instances WHERE id = ?1",
                params![row.instance_id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false);
        let plan = if !auto {
            "off"
        } else if continuations_since_progress(conn, &row.instance_id)? >= CONTINUATION_CAP {
            "capped"
        } else {
            "continue"
        };
        // §11.6: the reset fact first, whether or not autoContinue is on.
        let fact = crate::inbox::instance_fact(
            conn,
            remuda_protocol::InboxFactKind::SupplyReset,
            &row.instance_id,
            row.run,
            row.resets_at.clone(),
            json!({
                "window": row.window,
                "resetsAt": row.resets_at,
                "fireAt": row.fire_at,
                "source": row.source,
                "profileId": row.profile_id,
                "autoContinue": plan,
            }),
        )?;
        if matches!(
            crate::inbox::record_about(conn, &row.instance_id, fact)?,
            crate::inbox::Outcome::Recorded(_)
        ) {
            out.facts += 1;
        }
        let (next, reason) = match plan {
            "off" => (state::DONE, Some("auto-continue-off")),
            "capped" => (state::CAPPED, Some("cap")),
            _ => (state::HOLDING, None),
        };
        conn.execute(
            "UPDATE resume_on_reset SET state = ?2, reason = ?3, fired_at = ?4, updated_at = ?4
              WHERE id = ?1 AND state = 'waiting'",
            params![row.id, next, reason, now_rfc3339()],
        )?;
        tracing::info!(plan, "usage reset fired");
    }
    // A continuation held this long past its fire time (the instance never
    // idle, its host never back, its lineage never resumed) is given up, so
    // no row is evaluated forever.
    let expired = crate::config::rfc3339(now - MAX_WAIT);
    let gave_up = conn.execute(
        "UPDATE resume_on_reset SET state = 'done', reason = 'expired', updated_at = ?2
          WHERE state = 'holding' AND fire_at < ?1",
        params![expired, now_rfc3339()],
    )?;
    if gave_up > 0 {
        tracing::info!(rows = gave_up, "usage continuation given up: held too long");
    }
    let holding = rows(
        conn,
        &format!(
            "SELECT {ROW_COLUMNS} FROM resume_on_reset
              WHERE state = 'holding' AND fire_at <= ?1 ORDER BY fire_at, id"
        ),
        &now_at,
    )?;
    for row in holding {
        if try_continue(conn, &row)? {
            out.continued += 1;
        }
    }
    Ok(out)
}

/// Continuations of `instance_id` formed after its latest substantive
/// progress.
fn continuations_since_progress(conn: &Connection, instance_id: &str) -> Result<i64, StoreError> {
    let mark = load_tracker(conn, instance_id)?.progress_mark.unwrap_or(0);
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM resume_on_reset
          WHERE instance_id = ?1 AND state = 'continued' AND id > ?2",
        params![instance_id, mark],
        |row| row.get(0),
    )?)
}

/// Why a holding row's continuation is not formed yet. Logged as a category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hold {
    Busy,
    Interaction,
    InputAhead,
    HostOffline,
    HostLacksFence,
    InitiatorNotLive,
    /// An answer to the instance is being committed (an input hold).
    InputPending,
}

impl Hold {
    fn label(self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::Interaction => "pending-interaction",
            Self::InputAhead => "input-ahead",
            Self::HostOffline => "host-offline",
            Self::HostLacksFence => "host-lacks-fence",
            Self::InitiatorNotLive => "initiator-not-live",
            Self::InputPending => "input-pending",
        }
    }
}

/// Form a holding row's continuation when the instance and its initiator
/// lineage are ready; settle it `done` when the instance can take none, or
/// `capped`. Returns whether a command was formed.
fn try_continue(conn: &Connection, row: &Row) -> Result<bool, StoreError> {
    // Rows written carry the Hub's own time; the tick's `now` only decides
    // which rows are due.
    let now = now_rfc3339();
    let now = now.as_str();
    #[allow(clippy::type_complexity)]
    let instance: Option<(String, String, String, Option<String>, Option<String>, bool)> = conn
        .query_row(
            "SELECT host_id, lifecycle, activity, fenced_at, ended_at, auto_continue
               FROM instances WHERE id = ?1",
            params![row.instance_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()?;
    let finish = |state: &str, reason: &str| -> Result<bool, StoreError> {
        conn.execute(
            "UPDATE resume_on_reset SET state = ?2, reason = ?3, updated_at = ?4
              WHERE id = ?1 AND state = 'holding'",
            params![row.id, state, reason, now],
        )?;
        tracing::info!(state, reason, "usage resume settled without a continuation");
        Ok(false)
    };
    let Some((host_id, lifecycle, activity, fenced_at, ended_at, auto)) = instance else {
        return finish(state::DONE, "not-live");
    };
    if crate::herdr_claims::claimed_row(conn, &row.instance_id)? {
        return finish(state::DONE, "claimed-pane");
    }
    if fenced_at.is_some()
        || ended_at.is_some()
        || !matches!(lifecycle.as_str(), "ready" | "running")
    {
        return finish(state::DONE, "not-live");
    }
    if !auto {
        return finish(state::DONE, "auto-continue-off");
    }
    if continuations_since_progress(conn, &row.instance_id)? >= CONTINUATION_CAP {
        return finish(state::CAPPED, "cap");
    }
    // The initiator the continuation will carry. A row F re-attributed names
    // the live chapter; one released after F (a forwarded continuation the
    // Node refused as `fenced`) carries the generation it was formed with, so
    // when its lineage has moved on, follow it here as well: the wait belongs
    // to the lineage, not to the chapter that happened to be live then.
    let mut initiator = row.initiator.clone();
    if let Some((_, lineage, _)) = &initiator {
        let live = live_initiator(conn, lineage)?;
        if live != initiator && lineage_running(conn, lineage)? {
            initiator = live;
        }
        if let Some((chapter, _, generation)) = &initiator {
            conn.execute(
                "UPDATE resume_on_reset
                    SET initiator_instance_id = ?2, initiator_generation = ?3, updated_at = ?4
                  WHERE id = ?1 AND state = 'holding'",
                params![row.id, chapter, generation, now_rfc3339()],
            )?;
        }
    }
    let held_since: Option<String> = conn.query_row(
        "SELECT input_hold_at FROM resume_on_reset WHERE id = ?1",
        params![row.id],
        |r| r.get(0),
    )?;
    let input_held = held_since
        .as_deref()
        .and_then(crate::inbox::parse_at)
        .is_some_and(|at| OffsetDateTime::now_utc() - at < INPUT_HOLD);
    let hold = if input_held {
        Some(Hold::InputPending)
    } else if activity != "idle" {
        Some(Hold::Busy)
    } else if conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM interactions WHERE instance_id = ?1 AND state = 'pending')",
        params![row.instance_id],
        |r| r.get::<_, bool>(0),
    )? {
        Some(Hold::Interaction)
    } else if conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM commands
                         WHERE instance_id = ?1 AND state <> 'settled'
                           AND operation IN ('instance.send', 'instance.respond',
                                             'instance.keys', 'tty.write'))",
        params![row.instance_id],
        |r| r.get::<_, bool>(0),
    )? {
        Some(Hold::InputAhead)
    } else if !conn
        .query_row(
            "SELECT state = 'online' FROM hosts WHERE id = ?1",
            params![host_id],
            |r| r.get::<_, bool>(0),
        )
        .optional()?
        .unwrap_or(false)
    {
        Some(Hold::HostOffline)
    } else if !crate::fence::host_capable(conn, &host_id)? {
        // §6.4: Hub-origin input goes only to Nodes that advertise the fence.
        Some(Hold::HostLacksFence)
    } else if let Some((chapter, lineage, generation)) = &initiator {
        match live_initiator(conn, lineage)? {
            Some((live_chapter, _, live_generation))
                if &live_chapter == chapter
                    && live_generation == *generation
                    && lineage_running(conn, lineage)? =>
            {
                None
            }
            _ => Some(Hold::InitiatorNotLive),
        }
    } else {
        None
    };
    if let Some(hold) = hold {
        tracing::debug!(hold = hold.label(), "usage continuation held");
        return Ok(false);
    }
    let text = continuation_input(conn, row)?;
    let command_id = crate::config::new_id("cmd").map_err(|e| StoreError::Id(e.to_string()))?;
    let payload = json!({
        "instanceId": row.instance_id,
        "origin": "hub",
        "input": text,
    });
    // D-055: a new command with its id persisted now, with the Hub-internal
    // initiator of the subscriber lineage's live generation and no device
    // (§7.1), so F's step 4 covers it and no Hub crash can send it twice.
    conn.execute(
        "INSERT INTO commands
            (id, instance_id, host_id, operation, state, resolution, forwarded,
             payload_json, idempotency_key, created_at, updated_at,
             initiator_instance_id, initiator_lineage_id, initiator_generation)
         VALUES (?1, ?2, ?3, 'instance.send', 'queued', 'clear', 0, ?4, NULL, ?5, ?5, ?6, ?7, ?8)",
        params![
            command_id,
            row.instance_id,
            host_id,
            payload.to_string(),
            now,
            initiator.as_ref().map(|i| i.0.clone()),
            initiator.as_ref().map(|i| i.1.clone()),
            initiator.as_ref().map(|i| i.2),
        ],
    )?;
    let attempts = continuations_since_progress(conn, &row.instance_id)? + 1;
    conn.execute(
        "UPDATE resume_on_reset
            SET state = 'continued', command_id = ?2, attempts = ?3, continued_at = ?4,
                reason = NULL, updated_at = ?4
          WHERE id = ?1 AND state = 'holding'",
        params![row.id, command_id, attempts, now],
    )?;
    tracing::info!(
        attempts,
        kind = if text["text"] == CONTINUE_TEXT {
            "continue"
        } else {
            "original"
        },
        "usage continuation formed"
    );
    Ok(true)
}

fn lineage_running(conn: &Connection, lineage_id: &str) -> Result<bool, StoreError> {
    let state: Option<String> = conn
        .query_row(
            "SELECT state FROM lineages WHERE lineage_id = ?1",
            params![lineage_id],
            |row| row.get(0),
        )
        .optional()?;
    // A lineage without continuity is its one instance: live when it is.
    Ok(match state {
        Some(state) => state == "running",
        None => conn
            .query_row(
                "SELECT ended_at IS NULL AND fenced_at IS NULL FROM instances WHERE id = ?1",
                params![lineage_id],
                |row| row.get::<_, bool>(0),
            )
            .optional()?
            .unwrap_or(false),
    })
}

/// `continue` when the failed turn produced output; otherwise the failed
/// turn's own input again (its command's `input`), or `continue` when the
/// Hub holds no command for it.
fn continuation_input(conn: &Connection, row: &Row) -> Result<Value, StoreError> {
    let prompt = |text: &str| json!({ "type": "prompt", "text": text, "origin": "hub" });
    if row.turn_output {
        return Ok(prompt(CONTINUE_TEXT));
    }
    let Some(command) = row.turn_command_id.as_deref() else {
        return Ok(prompt(CONTINUE_TEXT));
    };
    let payload: Option<String> = conn
        .query_row(
            "SELECT payload_json FROM commands
              WHERE id = ?1 AND instance_id = ?2
                AND operation IN ('instance.send', 'instance.create', 'instance.resume')",
            params![command, row.instance_id],
            |r| r.get(0),
        )
        .optional()?;
    let input = payload
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|payload| original_prompt(&payload));
    Ok(match input {
        Some(mut input) => {
            input["origin"] = json!("hub");
            input
        }
        None => prompt(CONTINUE_TEXT),
    })
}

/// The prompt input a command carried, in every shape the Hub stores: a
/// send's `input` (`type: prompt`), a create's or resume's `initialInput`,
/// or a bare `prompt` / `text`. `None` when it carried no prompt text (keys,
/// an answer, a launch without a prompt).
fn original_prompt(payload: &Value) -> Option<Value> {
    for field in ["input", "initialInput"] {
        if let Some(input) = payload.get(field)
            && input.get("type").and_then(Value::as_str) == Some("prompt")
        {
            let has_text = input
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.trim().is_empty())
                || input
                    .get("blocks")
                    .and_then(Value::as_array)
                    .is_some_and(|blocks| !blocks.is_empty());
            if has_text {
                return Some(input.clone());
            }
        }
    }
    ["prompt", "text"]
        .iter()
        .filter_map(|field| payload.get(*field).and_then(Value::as_str))
        .find(|text| !text.trim().is_empty())
        .map(|text| json!({ "type": "prompt", "text": text }))
}

/// Cancel `instance_id`'s wait because input reached it (`reason`), from
/// any writer transaction: the `waiting` and `holding` rows, and a formed
/// continuation still queued and unforwarded (settled `cancelled`,
/// `resume-cancelled`). The `commands` trigger does the same for input that
/// passes through the command table; this is for the paths that do not —
/// an answered interaction goes straight to its Node as a direct RPC
/// (§7.5) and never inserts a command row. The only other input routes
/// (`instance.send`, `instance.respond`, `instance.keys`, `tty.write`,
/// `instance.configure`, and the worker keys/answer verbs that post them)
/// all write command rows and are already covered by the trigger.
pub(crate) fn cancel(
    conn: &Connection,
    instance_id: &str,
    reason: &str,
) -> Result<usize, StoreError> {
    let now = now_rfc3339();
    conn.execute(
        "UPDATE commands
            SET state = 'settled', resolution = 'clear',
                settlement_outcome = 'cancelled', settlement_reason = 'resume-cancelled',
                updated_at = ?2
          WHERE id IN (SELECT command_id FROM resume_on_reset
                        WHERE instance_id = ?1 AND state = 'continued')
            AND state = 'queued' AND forwarded = 0",
        params![instance_id, now],
    )?;
    Ok(conn.execute(
        "UPDATE resume_on_reset
            SET state = 'cancelled', reason = ?2, updated_at = ?3
          WHERE instance_id = ?1
            AND (state IN ('waiting', 'holding')
                 OR (state = 'continued' AND EXISTS (
                       SELECT 1 FROM commands c
                        WHERE c.id = resume_on_reset.command_id
                          AND c.settlement_reason = 'resume-cancelled')))",
        params![instance_id, reason, now],
    )?)
}

/// Phase one of an answer's cancel (`on`), or the release of a hold whose
/// answer did not commit (`!on`): the instance's open waits form no
/// continuation while held, for at most [`INPUT_HOLD`].
pub(crate) fn hold_for_input(
    conn: &Connection,
    instance_id: &str,
    on: bool,
) -> Result<usize, StoreError> {
    let now = now_rfc3339();
    Ok(conn.execute(
        "UPDATE resume_on_reset SET input_hold_at = ?2, updated_at = ?3
          WHERE instance_id = ?1 AND state IN ('waiting', 'holding')",
        params![instance_id, on.then_some(now.as_str()), now],
    )?)
}

/// F (§7.2) moved `lineage_id` to `chapter` at `generation`: every wait that
/// carries the lineage's initiator is re-attributed to it. That includes a
/// `continued` row whose continuation is already forwarded: F settles only
/// unforwarded work, so the Node may still refuse that command as `fenced`
/// afterwards, and the release must land on the successor's initiator rather
/// than on a generation no longer live. Runs inside F's transaction, after
/// step 4 released a continuation it cancelled.
pub(crate) fn reattribute(
    conn: &Connection,
    lineage_id: &str,
    chapter: &str,
    generation: i64,
) -> Result<usize, StoreError> {
    Ok(conn.execute(
        "UPDATE resume_on_reset
            SET initiator_instance_id = ?2, initiator_generation = ?3, updated_at = ?4
          WHERE initiator_lineage_id = ?1
            AND state IN ('waiting', 'holding', 'continued')",
        params![lineage_id, chapter, generation, now_rfc3339()],
    )?)
}

/// The command ids of continuations formed and not yet forwarded, with
/// their hosts, oldest first (the delivery driver forwards them).
pub(crate) const UNSENT_SQL: &str = "SELECT r.command_id, c.host_id, r.continued_at
       FROM resume_on_reset r JOIN commands c ON c.id = r.command_id
      WHERE r.state = 'continued' AND c.state = 'queued' AND c.forwarded = 0";

impl crate::store::Store {
    /// [`cancel`] as its own writer job, for a caller whose settlement is not
    /// one (a live-only approval grant has no durable row to join).
    /// [`hold_for_input`] as its own writer job.
    pub(crate) async fn hold_usage_wait(
        &self,
        instance_id: String,
        on: bool,
    ) -> Result<usize, StoreError> {
        self.run_named("hold_usage_wait", move |conn| {
            hold_for_input(conn, &instance_id, on)
        })
        .await
    }

    pub(crate) async fn cancel_usage_wait(
        &self,
        instance_id: String,
        reason: &'static str,
    ) -> Result<usize, StoreError> {
        self.run_named("cancel_usage_wait", move |conn| {
            cancel(conn, &instance_id, reason)
        })
        .await
    }

    /// Set `instance_id`'s `autoContinue`; `false` when there is no such
    /// instance. Turning it off settles nothing already formed: a
    /// continuation formed before still goes (it is a command with its id).
    pub(crate) async fn set_auto_continue(
        &self,
        instance_id: String,
        auto_continue: bool,
    ) -> Result<bool, StoreError> {
        self.run_named("set_auto_continue", move |conn| {
            Ok(conn.execute(
                "UPDATE instances SET auto_continue = ?2, updated_at = ?3 WHERE id = ?1",
                params![instance_id, auto_continue, now_rfc3339()],
            )? > 0)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(raw: &str) -> OffsetDateTime {
        OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339).unwrap()
    }

    #[test]
    fn only_subscription_windows_wait() {
        let rejected = Evidence::WindowRejected {
            window: "five_hour".into(),
            resets_at: Some(1),
        };
        for credential in [Credential::Subscription, Credential::HostLogin] {
            assert_eq!(classify(credential, &rejected), Class::Subscription);
            assert_eq!(
                classify(credential, &Evidence::SubscriptionNotice),
                Class::Subscription
            );
            assert_eq!(
                classify(credential, &Evidence::ApiRateLimit),
                Class::ApiRateLimit
            );
            assert_eq!(classify(credential, &Evidence::None), Class::NotALimit);
        }
        // An API key or a gateway never waits, whatever its evidence says.
        for evidence in [
            rejected,
            Evidence::SubscriptionNotice,
            Evidence::ApiRateLimit,
        ] {
            assert_eq!(
                classify(Credential::ApiKeyOrGateway, &evidence),
                Class::ApiKeyOrGateway
            );
        }
        assert!(is_subscription_notice(
            "You\u{2019}ve hit your limit · resets 3pm (Asia/Shanghai)"
        ));
        assert!(is_subscription_notice("You've hit your weekly limit"));
        assert!(!is_subscription_notice(
            "API Error: 429 {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\"}}"
        ));
        assert!(is_api_rate_limit(
            "API Error: 429 {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\"}} retry after 30s"
        ));
    }

    #[test]
    fn a_text_time_counts_only_with_its_zone() {
        let observed = at("2026-11-03T05:10:00Z");
        // 13:10 in Shanghai; 3pm there is 07:00Z the same day.
        assert_eq!(
            reset_in_text(
                "You've hit your limit · resets 3pm (Asia/Shanghai)",
                observed
            ),
            Some(at("2026-11-03T07:00:00Z"))
        );
        // Already past today in that zone: tomorrow.
        assert_eq!(
            reset_in_text(
                "You've hit your limit · resets 1pm (Asia/Shanghai)",
                observed
            ),
            Some(at("2026-11-04T05:00:00Z"))
        );
        assert_eq!(
            reset_in_text("limit reached, resets 2:30pm (UTC)", observed),
            Some(at("2026-11-03T14:30:00Z"))
        );
        assert_eq!(
            reset_in_text("resets at 15:00 UTC+8", observed),
            Some(at("2026-11-03T07:00:00Z"))
        );
        assert_eq!(
            reset_in_text(
                "You've hit your weekly limit · resets Nov 5, 9am (America/New_York)",
                observed
            ),
            Some(at("2026-11-05T14:00:00Z"))
        );
        assert_eq!(
            reset_in_text("weekly limit; resets Jan 2 at 9am (UTC)", observed),
            Some(at("2027-01-02T09:00:00Z"))
        );
        // A dated time earlier today is next year's occurrence, never a past
        // instant that would fire at once (r2 item 4).
        assert_eq!(
            reset_in_text("weekly limit; resets Nov 3, 1am (UTC)", observed),
            Some(at("2027-11-03T01:00:00Z"))
        );
        // Later today stays today.
        assert_eq!(
            reset_in_text("weekly limit; resets Nov 3, 11pm (UTC)", observed),
            Some(at("2026-11-03T23:00:00Z"))
        );
        assert_eq!(
            reset_in_text("quota resets 2026-11-03T09:00:00+01:00", observed),
            Some(at("2026-11-03T08:00:00Z"))
        );
        // No zone stated: never guessed.
        for text in [
            "5-hour limit reached ∙ resets 3am",
            "You've hit your limit · resets 3pm",
            "resets 3pm (local time)",
            "resets 3pm (Mars/Olympus)",
            "retry after 30 seconds",
        ] {
            assert_eq!(reset_in_text(text, observed), None, "{text}");
        }
    }
}
