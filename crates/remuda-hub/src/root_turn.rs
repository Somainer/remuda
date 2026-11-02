//! D-060 decision 1 (cb-rootturn): the root turn an Agent call was made in.
//!
//! **A record, never a gate (owner ruling R2).** No admission, grant, scope,
//! approval or fence rule reads anything in this module. It answers one
//! question for the audit trail and the PWA: which input started the turn the
//! calling instance is in, and who authored that input.
//!
//! The Hub derives the answer from its own records, never from a request body:
//!
//! * the `commands` row of each input (its stamped `origin`, its initiator);
//! * the instance journal as it is appended: the user message that names the
//!   delivering `commandId`, and — on the structured carriers (claude-sdk,
//!   claude-print) — the `turn/turn_started` lifecycle whose
//!   `nativeClientMessageId` is the delivering command id, plus the
//!   `turn/result` that names the turn it completed (`completedTurn`) or
//!   settles the root (`settledRootTurn`).
//!
//! A small per-instance ledger (`instances.root_turn_json`) is folded in the
//! journal append transaction, so it is exactly as durable as the rows it
//! summarises and costs one row read per Agent call.
//!
//! Attribution:
//!
//! * a structured carrier with an open turn: the front turn's input, exact
//!   (`continuation: none`);
//! * a structured carrier with no open turn (the carrier started a turn with
//!   no new input, e.g. a background task finished): the latest input,
//!   `origin: agent`, `continuation: native`;
//! * a carrier without turn boundaries: the latest input delivered, with its
//!   own origin, `continuation: unknown`;
//! * an input the Hub authored (the restart notice, stamped `origin: hub`,
//!   or any row no caller sent): `origin: hub`, `continuation: hub`
//!   (D-057 §6.4).
//!
//! * after the process that ran the turn ended — terminal native session
//!   evidence (`exited`, `failed`, `carrier_closed`), an instance lifecycle
//!   into `starting`, `closing`, `exited` or `failed`, a native session id
//!   other than the one the open turns ran on, or an instance row that is no
//!   longer running — open turns are dropped and the latest input reads as
//!   `origin: agent`, `continuation: unknown` until a new input arrives. A
//!   relaunched process never inherits the owner's turn.
//!
//! Known limit: the ledger is as current as the journal the Hub has ingested.
//! A call that races ahead of its own turn's `turn_started` is attributed from
//! the previous state, and that state can only read as more human than the
//! truth while the process that ran the owner's turn is still alive and its
//! `result` has not been ingested yet: the call then belongs to the owner's
//! turn, which has not ended.

use std::collections::HashMap;

use remuda_protocol::{InputOrigin, RequestedBy, RootTurn, RootTurnContinuation};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::store::{StoreError, ensure_column};

/// Open structured turns kept per instance. A carrier never has more than a
/// handful of prompts written ahead; the bound only stops a pathological
/// journal from growing the row.
const MAX_OPEN_TURNS: usize = 64;

/// Longest owner excerpt recorded as `requestedBy.excerpt` (cb-requested-by).
pub(crate) const EXCERPT_CHARS: usize = 280;

/// Columns for the ledger and for the root turn stamped on command rows and
/// audit rows. Plain additive columns, so no `user_version` rung.
pub(crate) fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    ensure_column(conn, "instances", "root_turn_json", "TEXT")?;
    ensure_column(conn, "commands", "initiator_root_turn_json", "TEXT")?;
    ensure_column(conn, "audit_log", "initiator_json", "TEXT")?;
    Ok(())
}

/// One input the ledger knows, classified from its Hub command row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Input {
    command_id: String,
    origin: InputOrigin,
    /// The Hub authored the input (no caller origin and no initiator).
    #[serde(default)]
    hub: bool,
}

impl Input {
    fn root_turn(&self, continuation: RootTurnContinuation) -> RootTurn {
        if self.hub {
            return RootTurn {
                command_id: self.command_id.clone(),
                origin: InputOrigin::Hub,
                continuation: RootTurnContinuation::Hub,
            };
        }
        RootTurn {
            command_id: self.command_id.clone(),
            origin: self.origin,
            continuation,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Ledger {
    /// Structured turns started for a known input and not yet completed,
    /// oldest (the one the carrier is running) first.
    #[serde(default)]
    open: Vec<Input>,
    /// The latest input delivered to the instance.
    #[serde(default)]
    last: Option<Input>,
    /// The carrier reported a turn start bound to an input: its boundaries
    /// are exact.
    #[serde(default)]
    exact: bool,
    /// Native session id the open turns run on (the structured carrier's
    /// `nativeId`). A turn or session record naming another one is a new
    /// process: the open turns cannot still be running.
    #[serde(default)]
    session: Option<String>,
    /// The process that received `last` has ended and no input has arrived
    /// since: whatever runs now was not started by that input.
    #[serde(default)]
    ended: bool,
}

impl Ledger {
    fn root_turn(&self) -> Option<RootTurn> {
        if self.ended {
            let last = self.last.as_ref()?;
            return Some(RootTurn {
                command_id: last.command_id.clone(),
                origin: InputOrigin::Agent,
                continuation: RootTurnContinuation::Unknown,
            });
        }
        if let Some(front) = self.open.first() {
            return Some(front.root_turn(RootTurnContinuation::Fresh));
        }
        let last = self.last.as_ref()?;
        if self.exact {
            // No input-started turn is open, so the carrier is running a turn
            // of its own: it continues the latest input, and no new input
            // authored it.
            let mut turn = last.root_turn(RootTurnContinuation::Native);
            if !last.hub {
                turn.origin = InputOrigin::Agent;
            }
            return Some(turn);
        }
        Some(last.root_turn(RootTurnContinuation::Unknown))
    }

    /// The process the ledger's turns ran on is gone: nothing open survives
    /// it, and its boundaries say nothing about the next process.
    fn end_process(&mut self) {
        self.open.clear();
        self.exact = false;
        self.ended = true;
    }

    /// Note the native session a turn or session record names. A different
    /// session than the open turns ran on ends them. Returns whether the
    /// ledger changed.
    fn observe_session(&mut self, session: Option<&str>) -> bool {
        let Some(session) = session.filter(|id| !id.is_empty()) else {
            return false;
        };
        match self.session.as_deref() {
            Some(known) if known == session => false,
            Some(_) => {
                self.end_process();
                self.session = Some(session.to_owned());
                true
            }
            None => {
                self.session = Some(session.to_owned());
                true
            }
        }
    }
}

/// Native session statuses that say the process is gone.
fn session_ended(status: Option<&str>) -> bool {
    matches!(status, Some("exited" | "failed" | "carrier_closed"))
}

/// Instance lifecycle states that say the process running the turns is gone
/// or being replaced.
fn instance_ended(state: Option<&str>) -> bool {
    matches!(state, Some("starting" | "closing" | "exited" | "failed"))
}

fn load(conn: &Connection, instance_id: &str) -> Result<Ledger, StoreError> {
    Ok(load_with_lifecycle(conn, instance_id)?.0)
}

/// The ledger and the instance row's lifecycle.
fn load_with_lifecycle(
    conn: &Connection,
    instance_id: &str,
) -> Result<(Ledger, Option<String>), StoreError> {
    let raw: Option<(Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT root_turn_json, lifecycle FROM instances WHERE id = ?1",
            params![instance_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (ledger, lifecycle) = raw.unwrap_or_default();
    Ok((
        ledger
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default(),
        lifecycle,
    ))
}

fn save(conn: &Connection, instance_id: &str, ledger: &Ledger) -> Result<(), StoreError> {
    let text = serde_json::to_string(ledger).map_err(|e| StoreError::Id(e.to_string()))?;
    conn.execute(
        "UPDATE instances SET root_turn_json = ?1 WHERE id = ?2",
        params![text, instance_id],
    )?;
    Ok(())
}

/// Classify the input `command_id` delivered to `instance_id` from the Hub's
/// own command row. `None` when the row is unknown or belongs to another
/// instance: a journal row can name a command, never invent one.
fn input_for(
    conn: &Connection,
    instance_id: &str,
    command_id: &str,
) -> Result<Option<Input>, StoreError> {
    let row: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT payload_json, initiator_instance_id FROM commands
              WHERE id = ?1 AND instance_id = ?2",
            params![command_id, instance_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((payload, initiator)) = row else {
        return Ok(None);
    };
    let stamped = serde_json::from_str::<Value>(&payload)
        .ok()
        .and_then(|payload| {
            payload
                .get("origin")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    // Input the Hub wrote is stamped `hub` (D-057 §6.4: the restart notice,
    // which carries its successor's initiator). Otherwise an initiator means
    // an Agent caller (or the Hub acting on one's behalf), and without one
    // the origin `stamp()` wrote decides. A row with neither was written by
    // the Hub itself.
    let (origin, hub) = if stamped.as_deref() == Some("hub") {
        (InputOrigin::Hub, true)
    } else if initiator.is_some() {
        (InputOrigin::Agent, false)
    } else {
        match stamped.as_deref() {
            Some("human") => (InputOrigin::Human, false),
            Some("bot") => (InputOrigin::Bot, false),
            Some(_) => (InputOrigin::Agent, false),
            None => (InputOrigin::Agent, true),
        }
    };
    Ok(Some(Input {
        command_id: command_id.to_owned(),
        origin,
        hub,
    }))
}

/// Fold one appended journal event into the instance's ledger. Runs in the
/// append transaction; events that say nothing about inputs or turns cost one
/// JSON inspection and no I/O.
pub(crate) fn fold(conn: &Connection, instance_id: &str, event: &Value) -> Result<(), StoreError> {
    let Some(payload) = event.get("payload") else {
        return Ok(());
    };
    match event.get("kind").and_then(Value::as_str) {
        Some("message") => {
            let user_input = payload.get("role").and_then(Value::as_str) == Some("user")
                && payload.get("operation").and_then(Value::as_str) == Some("open")
                && payload.get("parentToolCallId").is_none_or(Value::is_null);
            let Some(command_id) = payload.get("commandId").and_then(Value::as_str) else {
                return Ok(());
            };
            if !user_input {
                return Ok(());
            }
            let Some(input) = input_for(conn, instance_id, command_id)? else {
                return Ok(());
            };
            let mut ledger = load(conn, instance_id)?;
            if ledger.last.as_ref() != Some(&input) || ledger.ended {
                ledger.last = Some(input);
                ledger.ended = false;
                save(conn, instance_id, &ledger)?;
            }
        }
        Some("lifecycle") => {
            let text = |pointer: &str| payload.pointer(pointer).and_then(Value::as_str);
            match (text("/type"), text("/topic")) {
                (Some("entity"), _) => {
                    if text("/entityType") == Some("instance") && instance_ended(text("/state")) {
                        let mut ledger = load(conn, instance_id)?;
                        if !ledger.ended {
                            ledger.end_process();
                            save(conn, instance_id, &ledger)?;
                        }
                    }
                    return Ok(());
                }
                (Some("native"), Some("session")) => {
                    let mut ledger = load(conn, instance_id)?;
                    let mut changed = ledger.observe_session(text("/nativeId/value"));
                    if session_ended(text("/status/value")) && !ledger.ended {
                        ledger.end_process();
                        changed = true;
                    }
                    if changed {
                        save(conn, instance_id, &ledger)?;
                    }
                    return Ok(());
                }
                (Some("native"), Some("turn")) => {}
                _ => return Ok(()),
            }
            let session = text("/nativeId/value");
            let related = |key: &str| {
                payload
                    .pointer(&format!("/relatedIds/{key}"))
                    .and_then(Value::as_str)
            };
            match payload.get("nativeName").and_then(Value::as_str) {
                Some("turn_started") => {
                    let input = match related("nativeClientMessageId") {
                        Some(command_id) => input_for(conn, instance_id, command_id)?,
                        None => None,
                    };
                    let mut ledger = load(conn, instance_id)?;
                    let changed = ledger.observe_session(session);
                    let Some(input) = input else {
                        if changed {
                            save(conn, instance_id, &ledger)?;
                        }
                        return Ok(());
                    };
                    let command_id = input.command_id.clone();
                    if !ledger.open.iter().any(|open| open.command_id == command_id) {
                        if ledger.open.len() >= MAX_OPEN_TURNS {
                            ledger.open.remove(0);
                        }
                        ledger.open.push(input.clone());
                    }
                    ledger.last = Some(input);
                    ledger.exact = true;
                    ledger.ended = false;
                    save(conn, instance_id, &ledger)?;
                }
                Some("result") => {
                    let completed = related("completedTurn");
                    let settled = related("settledRootTurn") == Some("true");
                    let mut ledger = load(conn, instance_id)?;
                    let session_changed = ledger.observe_session(session);
                    if completed.is_none() && !settled {
                        if session_changed {
                            save(conn, instance_id, &ledger)?;
                        }
                        return Ok(());
                    }
                    let before = ledger.open.len();
                    if let Some(completed) = completed
                        && let Some(at) = ledger
                            .open
                            .iter()
                            .position(|open| open.command_id == completed)
                    {
                        ledger.open.remove(at);
                    }
                    if settled {
                        ledger.open.clear();
                    }
                    if ledger.open.len() != before || session_changed {
                        save(conn, instance_id, &ledger)?;
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
    Ok(())
}

/// The root turn `instance_id` is in now, as far as the Hub's records say.
///
/// An instance row that is no longer running (a Hub-side reconcile can move
/// it without a journal row) reads as ended, like the journal evidence does.
pub(crate) fn derive(conn: &Connection, instance_id: &str) -> Result<Option<RootTurn>, StoreError> {
    let (mut ledger, lifecycle) = load_with_lifecycle(conn, instance_id)?;
    if matches!(lifecycle.as_deref(), Some("closing" | "exited" | "failed")) {
        ledger.end_process();
    }
    Ok(ledger.root_turn())
}

/// Serialize a root turn for a `*_json` column.
pub(crate) fn to_column(root_turn: Option<&RootTurn>) -> Option<String> {
    root_turn.and_then(|turn| serde_json::to_string(turn).ok())
}

/// Read a root turn back from a `*_json` column; unreadable reads as absent.
pub(crate) fn from_column(text: Option<String>) -> Option<RootTurn> {
    text.and_then(|text| serde_json::from_str(&text).ok())
}

/// cb-requested-by: the `requestedBy` record for one root turn, taken from
/// the Hub's own command row and never from a request body.
///
/// An owner-origin turn records the command that carried the input plus a
/// bounded, secret-scrubbed excerpt of the owner's words. Every other origin
/// records only that origin (`hub` for a restart notice or a fact, `agent`
/// for a turn the seat started on its own). Record only, never a gate.
pub(crate) fn requested_by(conn: &Connection, turn: &RootTurn) -> Result<RequestedBy, StoreError> {
    if turn.origin != InputOrigin::Human {
        return Ok(RequestedBy {
            command_id: None,
            excerpt: None,
            origin: Some(turn.origin),
        });
    }
    let excerpt =
        owner_excerpts(conn, std::slice::from_ref(&turn.command_id))?.remove(&turn.command_id);
    Ok(RequestedBy {
        command_id: Some(turn.command_id.clone()),
        excerpt,
        origin: None,
    })
}

/// cb-requested-by: the owner's own words for each of `command_ids`, read from
/// the Hub's command rows: one line, credential-shaped words withheld, then at
/// most [`EXCERPT_CHARS`] characters cut on a char boundary. Attachment
/// contents are never in the prompt text.
///
/// Only an owner's input yields words. A row an Agent sent (it carries an
/// initiator) or one not stamped `human` yields none, so a record can never
/// put another author's text under the owner's name. Ids with no such row, or
/// no prompt text, are absent from the map.
pub(crate) fn owner_excerpts(
    conn: &Connection,
    command_ids: &[String],
) -> Result<HashMap<String, String>, StoreError> {
    let mut statement = conn
        .prepare_cached("SELECT payload_json, initiator_instance_id FROM commands WHERE id = ?1")?;
    let mut excerpts = HashMap::new();
    for id in command_ids {
        if excerpts.contains_key(id) {
            continue;
        }
        let row: Option<(String, Option<String>)> = statement
            .query_row(params![id], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;
        let Some((payload, None)) = row else {
            continue;
        };
        let Ok(payload) = serde_json::from_str::<Value>(&payload) else {
            continue;
        };
        if payload.get("origin").and_then(Value::as_str) != Some("human") {
            continue;
        }
        if let Some(excerpt) = prompt_text(&payload)
            .map(|text| clip_excerpt(&crate::digest::redact_secrets(&text)))
            .filter(|text| !text.is_empty())
        {
            excerpts.insert(id.clone(), excerpt);
        }
    }
    Ok(excerpts)
}

/// One line of at most [`EXCERPT_CHARS`] characters, cut on a char boundary.
fn clip_excerpt(text: &str) -> String {
    if text.chars().count() <= EXCERPT_CHARS {
        return text.to_string();
    }
    let mut clipped: String = text.chars().take(EXCERPT_CHARS - 1).collect();
    clipped.push('…');
    clipped
}

/// The prompt text of a stored input, mirroring the Node's `prompt_of`
/// (`runtime_wss.rs`): `initialInput.text` (a create's opening prompt),
/// `prompt` (the web), `spec.prompt`, `input.text` (Hub-authored inputs),
/// `text`, then the `input.blocks` text parts joined by newlines.
fn prompt_text(payload: &Value) -> Option<String> {
    if let Some(text) = payload
        .pointer("/initialInput/text")
        .or_else(|| payload.get("prompt"))
        .or_else(|| payload.pointer("/spec/prompt"))
        .or_else(|| payload.pointer("/input/text"))
        .or_else(|| payload.get("text"))
        .and_then(Value::as_str)
    {
        return Some(text.to_owned());
    }
    let blocks = payload.pointer("/input/blocks")?.as_array()?;
    let mut out = String::new();
    for block in blocks {
        let Some(text) = block.get("text").and_then(Value::as_str) else {
            continue;
        };
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(text);
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE instances (id TEXT PRIMARY KEY, lifecycle TEXT);
             CREATE TABLE commands (id TEXT PRIMARY KEY, instance_id TEXT,
                 payload_json TEXT NOT NULL, initiator_instance_id TEXT);
             CREATE TABLE audit_log (id INTEGER PRIMARY KEY);
             INSERT INTO instances (id) VALUES ('ins_main');",
        )
        .unwrap();
        migrate(&conn).unwrap();
        conn
    }

    fn command(conn: &Connection, id: &str, payload: Value, initiator: Option<&str>) {
        conn.execute(
            "INSERT INTO commands (id, instance_id, payload_json, initiator_instance_id)
             VALUES (?1, 'ins_main', ?2, ?3)",
            params![id, payload.to_string(), initiator],
        )
        .unwrap();
    }

    fn user_message(command_id: &str) -> Value {
        json!({"kind": "message", "payload": {
            "operation": "open", "messageId": format!("msg_{command_id}"),
            "role": "user", "parentToolCallId": null, "commandId": command_id,
            "blocks": [{"type": "text", "text": "hi"}]
        }})
    }

    fn turn(native_name: &str, related: Value) -> Value {
        json!({"kind": "lifecycle", "payload": {
            "type": "native", "topic": "turn", "nativeName": native_name,
            "relatedIds": related
        }})
    }

    fn started(command_id: &str) -> Value {
        turn("turn_started", json!({"nativeClientMessageId": command_id}))
    }

    fn fold_all(conn: &Connection, events: &[Value]) {
        for event in events {
            fold(conn, "ins_main", event).unwrap();
        }
    }

    fn now(conn: &Connection) -> Option<RootTurn> {
        derive(conn, "ins_main").unwrap()
    }

    fn rt(command_id: &str, origin: InputOrigin, continuation: RootTurnContinuation) -> RootTurn {
        RootTurn {
            command_id: command_id.into(),
            origin,
            continuation,
        }
    }

    #[test]
    fn an_owner_send_on_a_structured_carrier_is_an_exact_human_turn() {
        let conn = db();
        command(&conn, "cmd_owner", json!({"origin": "human"}), None);
        assert_eq!(now(&conn), None, "no input yet");
        fold_all(&conn, &[user_message("cmd_owner"), started("cmd_owner")]);
        assert_eq!(
            now(&conn),
            Some(rt(
                "cmd_owner",
                InputOrigin::Human,
                RootTurnContinuation::Fresh
            ))
        );
    }

    #[test]
    fn a_turn_with_no_new_input_continues_the_last_one_as_agent() {
        let conn = db();
        command(&conn, "cmd_owner", json!({"origin": "human"}), None);
        fold_all(
            &conn,
            &[
                user_message("cmd_owner"),
                started("cmd_owner"),
                // The owner's turn completes, but the carrier has its own
                // follow-up queued (a background task finished).
                turn(
                    "result",
                    json!({"completedTurn": "cmd_owner", "queuedTurnCount": "1"}),
                ),
            ],
        );
        assert_eq!(
            now(&conn),
            Some(rt(
                "cmd_owner",
                InputOrigin::Agent,
                RootTurnContinuation::Native
            ))
        );
    }

    #[test]
    fn queued_inputs_hand_the_root_over_exactly_at_each_completion() {
        let conn = db();
        command(&conn, "cmd_owner", json!({"origin": "human"}), None);
        command(
            &conn,
            "cmd_peer",
            json!({"origin": "agent"}),
            Some("ins_peer"),
        );
        fold_all(
            &conn,
            &[
                user_message("cmd_owner"),
                started("cmd_owner"),
                user_message("cmd_peer"),
                started("cmd_peer"),
            ],
        );
        // The owner's turn is still the one running, although a newer input
        // was delivered.
        assert_eq!(
            now(&conn),
            Some(rt(
                "cmd_owner",
                InputOrigin::Human,
                RootTurnContinuation::Fresh
            ))
        );
        fold_all(
            &conn,
            &[turn("result", json!({"completedTurn": "cmd_owner"}))],
        );
        assert_eq!(
            now(&conn),
            Some(rt(
                "cmd_peer",
                InputOrigin::Agent,
                RootTurnContinuation::Fresh
            ))
        );
        fold_all(
            &conn,
            &[turn(
                "result",
                json!({"completedTurn": "cmd_peer", "settledRootTurn": "true"}),
            )],
        );
        assert_eq!(
            now(&conn),
            Some(rt(
                "cmd_peer",
                InputOrigin::Agent,
                RootTurnContinuation::Native
            )),
            "after the settle, a further call is the agent continuing on its own"
        );
    }

    #[test]
    fn a_hub_authored_input_is_hub_with_a_hub_continuation() {
        let conn = db();
        command(
            &conn,
            "cmd_fact",
            json!({"input": {"type": "prompt"}}),
            None,
        );
        fold_all(&conn, &[user_message("cmd_fact"), started("cmd_fact")]);
        assert_eq!(
            now(&conn),
            Some(rt("cmd_fact", InputOrigin::Hub, RootTurnContinuation::Hub))
        );
    }

    /// D-057 §6.4: the restart notice is stamped `origin: hub` and carries
    /// its successor's initiator; it is still the Hub's input, never Agent.
    #[test]
    fn the_restart_notice_records_the_real_hub_origin() {
        let conn = db();
        command(
            &conn,
            "cmd_notice",
            json!({"origin": "hub", "input": {"type": "prompt", "origin": "hub"}}),
            Some("ins_main"),
        );
        fold_all(&conn, &[user_message("cmd_notice"), started("cmd_notice")]);
        assert_eq!(
            now(&conn),
            Some(rt(
                "cmd_notice",
                InputOrigin::Hub,
                RootTurnContinuation::Hub
            ))
        );
        // After the notice's turn settles, the seat continuing on its own
        // still traces back to the Hub's input.
        fold_all(
            &conn,
            &[turn(
                "result",
                json!({"completedTurn": "cmd_notice", "settledRootTurn": "true"}),
            )],
        );
        assert_eq!(now(&conn).map(|turn| turn.origin), Some(InputOrigin::Hub));
    }

    #[test]
    fn a_carrier_without_boundaries_records_the_latest_input_honestly() {
        let conn = db();
        command(&conn, "cmd_owner", json!({"origin": "human"}), None);
        command(&conn, "cmd_bot", json!({"origin": "bot"}), None);
        fold_all(&conn, &[user_message("cmd_owner")]);
        assert_eq!(
            now(&conn),
            Some(rt(
                "cmd_owner",
                InputOrigin::Human,
                RootTurnContinuation::Unknown
            ))
        );
        fold_all(&conn, &[user_message("cmd_bot")]);
        assert_eq!(
            now(&conn),
            Some(rt(
                "cmd_bot",
                InputOrigin::Bot,
                RootTurnContinuation::Unknown
            ))
        );
    }

    #[test]
    fn journal_rows_cannot_name_a_command_the_hub_does_not_hold() {
        let conn = db();
        command(&conn, "cmd_owner", json!({"origin": "human"}), None);
        conn.execute(
            "INSERT INTO commands (id, instance_id, payload_json) VALUES
             ('cmd_elsewhere', 'ins_other', '{\"origin\":\"human\"}')",
            [],
        )
        .unwrap();
        fold_all(
            &conn,
            &[
                started("cmd_invented"),
                started("cmd_elsewhere"),
                user_message("cmd_invented"),
            ],
        );
        assert_eq!(now(&conn), None, "nothing the Hub holds was named");
        // A subagent's user frame or a non-open mutation is not an input.
        let mut subagent = user_message("cmd_owner");
        subagent["payload"]["parentToolCallId"] = json!("tc_1");
        let mut replace = user_message("cmd_owner");
        replace["payload"]["operation"] = json!("replace");
        fold_all(&conn, &[subagent, replace]);
        assert_eq!(now(&conn), None);
    }

    /// A turn lifecycle on native session `session`, as the claude-sdk
    /// mapper journals it (`nativeId` is the session id).
    fn in_session(mut event: Value, session: &str) -> Value {
        event["payload"]["nativeId"] = json!({"state": "known", "value": session});
        event
    }

    fn session(status: &str, session: &str) -> Value {
        json!({"kind": "lifecycle", "payload": {
            "type": "native", "topic": "session", "nativeName": "session",
            "nativeId": {"state": "known", "value": session},
            "status": {"state": "known", "value": status}, "relatedIds": {}
        }})
    }

    fn instance_state(state: &str) -> Value {
        json!({"kind": "lifecycle", "payload": {
            "type": "entity", "entityType": "instance", "state": state,
            "reasonCode": "test"
        }})
    }

    /// The owner's turn is open on session `s1`, exactly as the RC journal
    /// orders it (the turn start lands before the session start).
    fn owner_turn_open(conn: &Connection) {
        command(conn, "cmd_owner", json!({"origin": "human"}), None);
        fold_all(
            conn,
            &[
                user_message("cmd_owner"),
                in_session(started("cmd_owner"), "s1"),
                session("started", "s1"),
            ],
        );
        assert_eq!(
            now(conn),
            Some(rt(
                "cmd_owner",
                InputOrigin::Human,
                RootTurnContinuation::Fresh
            )),
            "the session start of the same process keeps the owner's turn"
        );
    }

    fn agent_unknown() -> Option<RootTurn> {
        Some(rt(
            "cmd_owner",
            InputOrigin::Agent,
            RootTurnContinuation::Unknown,
        ))
    }

    /// r2 item 1: the process dies mid-turn (no result is journaled) and the
    /// carrier relaunches it on the same instance. Whatever the relaunched
    /// agent does next was not started by the owner.
    #[test]
    fn a_process_end_mid_turn_never_leaves_the_owner_turn_open() {
        for death in [
            session("exited", "s1"),
            session("failed", "s1"),
            session("carrier_closed", "s1"),
            instance_state("exited"),
            instance_state("failed"),
            instance_state("starting"),
        ] {
            let conn = db();
            owner_turn_open(&conn);
            fold_all(&conn, std::slice::from_ref(&death));
            assert_eq!(now(&conn), agent_unknown(), "after {death}");
            // The relaunch (resuming the same native session) starts no
            // input-bound turn: it is still not the owner's.
            fold_all(&conn, &[instance_state("ready"), session("started", "s1")]);
            assert_eq!(now(&conn), agent_unknown(), "relaunched after {death}");
        }
    }

    /// r2 item 1: a native session change (a relaunch on a new session, with
    /// no exit evidence journaled) clears the open turns.
    #[test]
    fn a_native_session_change_clears_the_open_turns() {
        let conn = db();
        owner_turn_open(&conn);
        fold_all(&conn, &[session("started", "s2")]);
        assert_eq!(now(&conn), agent_unknown());

        // A turn the new process starts on its own, on the new session.
        let conn = db();
        owner_turn_open(&conn);
        fold_all(&conn, &[in_session(turn("turn_started", json!({})), "s2")]);
        assert_eq!(now(&conn), agent_unknown());

        // A new input on the new session is attributed exactly again.
        command(&conn, "cmd_next", json!({"origin": "human"}), None);
        fold_all(
            &conn,
            &[
                user_message("cmd_next"),
                in_session(started("cmd_next"), "s2"),
            ],
        );
        assert_eq!(
            now(&conn),
            Some(rt(
                "cmd_next",
                InputOrigin::Human,
                RootTurnContinuation::Fresh
            ))
        );
    }

    /// r2 item 1: an instance row that is not running reads as ended even if
    /// no lifecycle row reached the journal (a Hub-side reconcile).
    #[test]
    fn an_instance_row_that_is_not_running_reads_as_ended() {
        let conn = db();
        conn.execute("UPDATE instances SET lifecycle = 'ready'", [])
            .unwrap();
        owner_turn_open(&conn);
        conn.execute("UPDATE instances SET lifecycle = 'exited'", [])
            .unwrap();
        assert_eq!(now(&conn), agent_unknown());
    }

    /// cb-requested-by: the record reads the owner's words off the Hub's own
    /// command row — bounded at a char boundary, one line, secret-shaped
    /// tokens withheld — and non-owner turns record only their origin.
    #[test]
    fn requested_by_reads_the_owners_words_from_the_command_row() {
        let conn = db();
        command(
            &conn,
            "cmd_owner",
            json!({"origin": "human", "prompt": "  dispatch a worker\non the other node "}),
            None,
        );
        let turn = rt("cmd_owner", InputOrigin::Human, RootTurnContinuation::Fresh);
        assert_eq!(
            requested_by(&conn, &turn).unwrap(),
            RequestedBy {
                command_id: Some("cmd_owner".into()),
                excerpt: Some("dispatch a worker on the other node".into()),
                origin: None,
            }
        );

        // A long body is cut at a char boundary (multi-byte chars included)
        // and still one line.
        let long = "他说：".repeat(120);
        command(
            &conn,
            "cmd_long",
            json!({"origin": "human", "text": long}),
            None,
        );
        let excerpt = requested_by(
            &conn,
            &rt("cmd_long", InputOrigin::Human, RootTurnContinuation::Fresh),
        )
        .unwrap()
        .excerpt
        .expect("excerpt");
        assert_eq!(excerpt.chars().count(), EXCERPT_CHARS, "{excerpt}");
        assert!(excerpt.ends_with('…'), "{excerpt}");

        // A credential-shaped word never reaches the record; the rest does.
        let secret = format!(
            "please use {} now",
            ["sk", "-live-abcd1234efgh5678ijkl"].concat()
        );
        command(
            &conn,
            "cmd_secret",
            json!({"origin": "human", "text": secret}),
            None,
        );
        let excerpt = requested_by(
            &conn,
            &rt(
                "cmd_secret",
                InputOrigin::Human,
                RootTurnContinuation::Fresh,
            ),
        )
        .unwrap()
        .excerpt
        .expect("excerpt");
        assert!(excerpt.contains("please use"), "{excerpt}");
        assert!(excerpt.contains("now"), "{excerpt}");
        assert!(!excerpt.contains("abcd1234"), "{excerpt}");

        // Every command shape the Node reads is understood, in the Node's
        // `prompt_of` order: a create's opening prompt, an explicit
        // `spec.prompt`, and `input.blocks`.
        for (id, payload, wanted) in [
            (
                "cmd_create",
                json!({"origin": "human",
                       "initialInput": {"type": "prompt", "text": "open the seat now"},
                       "spec": {"prompt": "spec words must not win"}}),
                "open the seat now",
            ),
            (
                "cmd_spec",
                json!({"origin": "human", "spec": {"prompt": "spec words only"}}),
                "spec words only",
            ),
            (
                "cmd_blocks",
                json!({"origin": "human", "input": {"type": "prompt", "blocks": [
                    {"type": "text", "text": "first"}, {"type": "text", "text": "second"}]}}),
                "first second",
            ),
        ] {
            command(&conn, id, payload, None);
            assert_eq!(
                requested_by(
                    &conn,
                    &rt(id, InputOrigin::Human, RootTurnContinuation::Fresh)
                )
                .unwrap()
                .excerpt
                .as_deref(),
                Some(wanted),
                "{id}"
            );
        }

        // A command with no prompt text still names the command, with no
        // excerpt invented for it.
        command(
            &conn,
            "cmd_keys",
            json!({"origin": "human", "keys": "enter"}),
            None,
        );
        assert_eq!(
            requested_by(
                &conn,
                &rt("cmd_keys", InputOrigin::Human, RootTurnContinuation::Fresh)
            )
            .unwrap(),
            RequestedBy {
                command_id: Some("cmd_keys".into()),
                excerpt: None,
                origin: None,
            }
        );

        // Non-owner origins record the origin alone, never the words.
        for origin in [InputOrigin::Agent, InputOrigin::Hub, InputOrigin::Bot] {
            assert_eq!(
                requested_by(
                    &conn,
                    &rt("cmd_owner", origin, RootTurnContinuation::Unknown)
                )
                .unwrap(),
                RequestedBy {
                    command_id: None,
                    excerpt: None,
                    origin: Some(origin),
                }
            );
        }
    }

    /// cb-requested-by: one batched read serves a whole list, and only an
    /// owner's input yields words — never a row an Agent sent or one not
    /// stamped `human`, whatever id a record names.
    #[test]
    fn owner_excerpts_read_only_owner_input_rows_in_one_batch() {
        let conn = db();
        command(
            &conn,
            "cmd_owner",
            json!({"origin": "human", "prompt": "ship it"}),
            None,
        );
        command(
            &conn,
            "cmd_other_owner",
            json!({"origin": "human", "input": {"type": "prompt", "text": "and this"}}),
            None,
        );
        // An Agent's send carries an initiator; a Bot's is stamped `bot`.
        command(
            &conn,
            "cmd_agent",
            json!({"origin": "agent", "prompt": "agent words"}),
            Some("ins_peer"),
        );
        command(
            &conn,
            "cmd_agent_claiming_human",
            json!({"origin": "human", "prompt": "forged"}),
            Some("ins_peer"),
        );
        command(
            &conn,
            "cmd_bot",
            json!({"origin": "bot", "prompt": "bot words"}),
            None,
        );
        let ids: Vec<String> = [
            "cmd_owner",
            "cmd_other_owner",
            "cmd_owner",
            "cmd_agent",
            "cmd_agent_claiming_human",
            "cmd_bot",
            "cmd_missing",
        ]
        .iter()
        .map(|id| id.to_string())
        .collect();
        let excerpts = owner_excerpts(&conn, &ids).unwrap();
        assert_eq!(
            excerpts,
            HashMap::from([
                ("cmd_owner".to_string(), "ship it".to_string()),
                ("cmd_other_owner".to_string(), "and this".to_string()),
            ])
        );
    }
}
