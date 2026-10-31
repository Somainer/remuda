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
//! * an input the Hub authored (no human, bot or agent caller sent it):
//!   `origin: agent`, `continuation: hub`, until ma-restart adds a `hub`
//!   origin.
//!
//! Known limit: the ledger is as current as the journal the Hub has ingested.
//! A call that races ahead of its own turn's `turn_started` is attributed from
//! the previous state, which can only read as less human, never more.

use remuda_protocol::{InputOrigin, RootTurn, RootTurnContinuation};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::store::{StoreError, ensure_column};

/// Open structured turns kept per instance. A carrier never has more than a
/// handful of prompts written ahead; the bound only stops a pathological
/// journal from growing the row.
const MAX_OPEN_TURNS: usize = 64;

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
                origin: InputOrigin::Agent,
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
}

impl Ledger {
    fn root_turn(&self) -> Option<RootTurn> {
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
}

fn load(conn: &Connection, instance_id: &str) -> Result<Ledger, StoreError> {
    let raw: Option<Option<String>> = conn
        .query_row(
            "SELECT root_turn_json FROM instances WHERE id = ?1",
            params![instance_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(raw
        .flatten()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default())
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
    // An initiator means an Agent caller (or the Hub acting on one's behalf);
    // otherwise the origin `stamp()` wrote decides. A row with neither was
    // written by the Hub itself.
    let (origin, hub) = if initiator.is_some() {
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
            if ledger.last.as_ref() != Some(&input) {
                ledger.last = Some(input);
                save(conn, instance_id, &ledger)?;
            }
        }
        Some("lifecycle") => {
            if payload.get("type").and_then(Value::as_str) != Some("native")
                || payload.get("topic").and_then(Value::as_str) != Some("turn")
            {
                return Ok(());
            }
            let related = |key: &str| {
                payload
                    .pointer(&format!("/relatedIds/{key}"))
                    .and_then(Value::as_str)
            };
            match payload.get("nativeName").and_then(Value::as_str) {
                Some("turn_started") => {
                    let Some(command_id) = related("nativeClientMessageId") else {
                        return Ok(());
                    };
                    let Some(input) = input_for(conn, instance_id, command_id)? else {
                        return Ok(());
                    };
                    let mut ledger = load(conn, instance_id)?;
                    if !ledger.open.iter().any(|open| open.command_id == command_id) {
                        if ledger.open.len() >= MAX_OPEN_TURNS {
                            ledger.open.remove(0);
                        }
                        ledger.open.push(input.clone());
                    }
                    ledger.last = Some(input);
                    ledger.exact = true;
                    save(conn, instance_id, &ledger)?;
                }
                Some("result") => {
                    let completed = related("completedTurn");
                    let settled = related("settledRootTurn") == Some("true");
                    if completed.is_none() && !settled {
                        return Ok(());
                    }
                    let mut ledger = load(conn, instance_id)?;
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
                    if ledger.open.len() != before {
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
pub(crate) fn derive(conn: &Connection, instance_id: &str) -> Result<Option<RootTurn>, StoreError> {
    Ok(load(conn, instance_id)?.root_turn())
}

/// Serialize a root turn for a `*_json` column.
pub(crate) fn to_column(root_turn: Option<&RootTurn>) -> Option<String> {
    root_turn.and_then(|turn| serde_json::to_string(turn).ok())
}

/// Read a root turn back from a `*_json` column; unreadable reads as absent.
pub(crate) fn from_column(text: Option<String>) -> Option<RootTurn> {
    text.and_then(|text| serde_json::from_str(&text).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE instances (id TEXT PRIMARY KEY);
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
    fn a_hub_authored_input_is_agent_with_a_hub_continuation() {
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
            Some(rt(
                "cmd_fact",
                InputOrigin::Agent,
                RootTurnContinuation::Hub
            ))
        );
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
}
