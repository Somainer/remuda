//! cb-command-queue (protocol.md §8.5; the D-055 2026-11-05 note): a caller
//! lists, edits and withdraws the commands it queued while they are provably
//! undelivered, and reads an instance's runtime config with its generation.
//!
//! * **Own** is the Hub-stamped initiator for an Agent (its instance or its
//!   lineage) and the queuing device for a Human or Bot; the unnarrowed owner
//!   sees every caller's. Hub-written input (`origin: hub`) belongs to no
//!   caller. Another caller's command answers 404.
//! * **Queued** is a Hub row that was never forwarded (`held: hub`, a CAS on
//!   `forwarded = 0` in the serial writer, which the forward-intent mark also
//!   runs in), or a prompt a PTY carrier's Node holds for a busy instance
//!   (`held: node`, the Node's first-wins claim against delivery). Anything
//!   else is 409 `already-delivered`: never guessed.
//! * **Generations.** The runtime-config generation is the instance's
//!   `configureSeq`; [`config_view`] is the one read of it, shared by
//!   `GET /v1/instances/{id}/config` and the 409 a stale `expectedGeneration`
//!   gets.
//!
//! Audit rows and logs carry categories and ids, never prompt text.

use crate::AppState;
use crate::agent_scope::{
    caller, origin, owns, require_approval, require_instance_read, shell_driver,
    target_in_caller_scope,
};
use crate::auth::require_origin;
use crate::error::HubError;
use crate::http::map_store;
use crate::store::{CommandRecord, Device, InstanceRecord, StoreError, ensure_column};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use remuda_protocol::InputOrigin;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

/// PTY carriers whose Node holds an accepted prompt while the instance is
/// busy (the Node's `pty_queue::is_pty`).
pub(crate) const HELD_INPUT_DRIVERS: &[&str] =
    &["claude-pty", "generic-pty", "shell-pty", "herdr-claimed"];
/// Carriers with no in-session effort channel: effort applies at launch.
pub(crate) const LAUNCH_EFFORT_DRIVERS: &[&str] = &["claude-print", "claude-sdk"];
/// A deferral that waits for the next idle moment of this chapter.
pub(crate) const NEXT_TURN: &str = "next-turn";
/// A deferral that waits for the next chapter's launch.
pub(crate) const NEXT_CHAPTER: &str = "next-chapter";
const LIST_DEFAULT: i64 = 50;
const LIST_MAX: i64 = 200;
/// Bound on the Node's held-input answer; the claim is in memory.
const NODE_CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Payload keys that carry an effort change.
const EFFORT_KEYS: &[&str] = &["effort", "effortName"];

/// The command columns `store::command_from_row` reads, table-qualified.
const COLUMNS: &str = "c.id, c.instance_id, c.host_id, c.operation, c.state, c.resolution,
        c.forwarded, c.payload_json, c.idempotency_key, c.created_at, c.updated_at,
        c.settlement_outcome, c.settlement_reason, c.settlement_http_status,
        c.settlement_http_body, c.initiator_instance_id, c.initiator_lineage_id,
        c.initiator_generation, c.initiator_device_id, c.initiator_root_turn_json,
        c.revision AS revision, c.original_payload_json AS original_payload_json,
        c.queued_by_device_id AS queued_by_device_id,
        c.withdrawn_by_json AS withdrawn_by_json, c.config_deferred AS config_deferred,
        c.config_deferred_resolved_at AS config_deferred_resolved_at";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/commands", get(list_commands))
        .route("/v1/commands/{id}", patch(edit_command))
        .route("/v1/commands/{id}/withdraw", post(withdraw_command))
        .route("/v1/instances/{id}/config", get(get_config))
}

/// Idempotent DDL, run with the other command-column migrations.
pub(crate) fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    ensure_column(conn, "commands", "revision", "INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(conn, "commands", "original_payload_json", "TEXT")?;
    ensure_column(conn, "commands", "queued_by_device_id", "TEXT")?;
    ensure_column(conn, "commands", "withdrawn_by_json", "TEXT")?;
    // `{prior, set}`: the values a configure replaced and the ones it set,
    // so withdrawing it restores exactly the fields it still holds.
    ensure_column(conn, "commands", "config_prior_json", "TEXT")?;
    ensure_column(conn, "commands", "config_deferred", "TEXT")?;
    ensure_column(conn, "commands", "config_deferred_resolved_at", "TEXT")?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS commands_open
             ON commands(state, operation, created_at DESC);",
    )
}

// ── who sees what ──────────────────────────────────────────────────────────

/// Whose commands a caller sees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Viewer {
    /// The unnarrowed owner without `mine=1`: every caller's.
    Owner,
    /// A Human or Bot device: the ones it queued.
    Device(String),
    /// An Agent: the ones its instance or its lineage initiated.
    Agent {
        instance_id: String,
        lineage_id: String,
    },
}

async fn viewer(state: &AppState, device: &Device, mine: bool) -> Result<Viewer, HubError> {
    match origin(device) {
        InputOrigin::Agent => {
            let instance_id = device.instance_id.clone().ok_or(HubError::Forbidden)?;
            let instance = state
                .store
                .get_instance(instance_id.clone())
                .await?
                .ok_or_else(HubError::stale_caller)?;
            Ok(Viewer::Agent {
                instance_id,
                lineage_id: instance.lineage_id,
            })
        }
        InputOrigin::Human if !mine => Ok(Viewer::Owner),
        _ => Ok(Viewer::Device(device.id.clone())),
    }
}

/// Input the Hub wrote itself (inbox fact batches, restart notices, wake
/// deliveries): nobody's to edit or withdraw.
fn hub_written(payload: &Value) -> bool {
    payload.get("origin").and_then(Value::as_str) == Some("hub")
        || payload.pointer("/input/origin").and_then(Value::as_str) == Some("hub")
}

/// Whether `viewer` may see and act on `row`.
pub(crate) fn visible(viewer: &Viewer, row: &CommandRecord) -> bool {
    if hub_written(&row.payload)
        || !matches!(
            row.operation.as_str(),
            "instance.send" | "instance.configure"
        )
    {
        return false;
    }
    match viewer {
        Viewer::Owner => true,
        Viewer::Device(device_id) => {
            row.initiator.is_none() && row.queued_by_device_id.as_deref() == Some(device_id)
        }
        Viewer::Agent {
            instance_id,
            lineage_id,
        } => row.initiator.as_ref().is_some_and(|initiator| {
            &initiator.instance_id == instance_id || &initiator.lineage_id == lineage_id
        }),
    }
}

/// Where a still-undelivered command waits, or `None` when it is not
/// provably undelivered. `node_holds` is whether the target's Node holds
/// prompts and advertised `capabilities.heldInput`.
pub(crate) fn held(row: &CommandRecord, node_holds: bool) -> Option<&'static str> {
    if row.state == "queued" && !row.forwarded {
        return Some("hub");
    }
    (row.state == "accepted" && row.operation == "instance.send" && node_holds).then_some("node")
}

/// Whether the instance's carrier holds prompts and its host advertised
/// `heldInput`.
fn node_holds_input(conn: &Connection, instance_id: &str) -> Result<bool, StoreError> {
    let drivers = HELD_INPUT_DRIVERS
        .iter()
        .map(|driver| format!("'{driver}'"))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(conn
        .query_row(
            &format!(
                "SELECT 1 FROM instances i JOIN hosts h ON h.id = i.host_id
                  WHERE i.id = ?1 AND i.driver IN ({drivers})
                    AND COALESCE(json_extract(h.capabilities_json, '$.heldInput'), 0) = 1"
            ),
            params![instance_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

// ── list ───────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListQuery {
    /// `1` / `true`: only the caller's own (implied for Agent and Bot).
    #[serde(default)]
    mine: Option<String>,
    /// Only `queued` is accepted; it is also the default.
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    instance_id: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

/// `GET /v1/commands?mine=1&state=queued`: the caller's undelivered sends
/// and configures, newest first, each with `revision` and `held`.
async fn list_commands(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, HubError> {
    let device = caller(&state, &headers).await?;
    if query
        .state
        .as_deref()
        .is_some_and(|value| value != "queued")
    {
        return Err(HubError::BadRequest(
            "state must be `queued` (the only listable state)".into(),
        ));
    }
    let mine = query
        .mine
        .as_deref()
        .is_some_and(|value| matches!(value, "1" | "true"));
    let viewer = viewer(&state, &device, mine).await?;
    let limit = query.limit.unwrap_or(LIST_DEFAULT).clamp(1, LIST_MAX);
    let instance_filter = query.instance_id.clone();
    let rows = state
        .store
        .read("list_queued_commands", {
            let viewer = viewer.clone();
            move |conn| list_queued(conn, &viewer, instance_filter.as_deref(), limit)
        })
        .await
        .map_err(map_store)?;
    Ok(Json(json!({ "commands": rows })))
}

pub(crate) fn list_queued(
    conn: &Connection,
    viewer: &Viewer,
    instance_id: Option<&str>,
    limit: i64,
) -> Result<Vec<CommandRecord>, StoreError> {
    let drivers = HELD_INPUT_DRIVERS
        .iter()
        .map(|driver| format!("'{driver}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let (agent_instance, agent_lineage, device) = match viewer {
        Viewer::Owner => (None, None, None),
        Viewer::Device(device) => (None, None, Some(device.clone())),
        Viewer::Agent {
            instance_id,
            lineage_id,
        } => (Some(instance_id.clone()), Some(lineage_id.clone()), None),
    };
    let agent = agent_instance.is_some();
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS},
                (i.driver IN ({drivers})
                 AND COALESCE(json_extract(h.capabilities_json, '$.heldInput'), 0) = 1)
           FROM commands c
           LEFT JOIN instances i ON i.id = c.instance_id
           LEFT JOIN hosts h ON h.id = c.host_id
          WHERE c.operation IN ('instance.send', 'instance.configure')
            AND ((c.state = 'queued' AND c.forwarded = 0)
                 OR (c.state = 'accepted' AND c.operation = 'instance.send'
                     AND i.driver IN ({drivers})
                     AND COALESCE(json_extract(h.capabilities_json, '$.heldInput'), 0) = 1))
            AND (?1 IS NULL OR c.instance_id = ?1)
            AND (?2 = 0 OR c.initiator_instance_id = ?3 OR c.initiator_lineage_id = ?4)
            AND (?5 IS NULL OR c.queued_by_device_id = ?5)
          ORDER BY c.created_at DESC, c.id DESC
          LIMIT ?6"
    ))?;
    let rows = stmt
        .query_map(
            params![
                instance_id,
                i64::from(agent),
                agent_instance,
                agent_lineage,
                device,
                limit
            ],
            |row| {
                let record = crate::store::command_from_row(row)?;
                let node_holds: Option<bool> = row.get(26)?;
                Ok((record, node_holds.unwrap_or(false)))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .filter(|(row, _)| visible(viewer, row))
        .filter_map(|(mut row, node_holds)| {
            row.held = Some(held(&row, node_holds)?.to_owned());
            Some(row)
        })
        .collect())
}

// ── shared admission for edit and withdraw ─────────────────────────────────

/// A JSON integer or decimal string (a U64 on the wire).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Revision {
    Number(u64),
    Text(String),
}

impl Revision {
    fn value(&self, field: &str) -> Result<i64, HubError> {
        let parsed = match self {
            Self::Number(number) => Some(*number),
            Self::Text(text) => text.trim().parse::<u64>().ok(),
        };
        parsed
            .and_then(|value| i64::try_from(value).ok())
            .ok_or_else(|| HubError::BadRequest(format!("{field} must be a non-negative integer")))
    }
}

/// A command the caller may act on, with its target.
struct Owned {
    row: CommandRecord,
    instance: InstanceRecord,
    held: Option<&'static str>,
}

/// Resolve `command_id` for an edit or withdraw by `device`: visible to it
/// (else 404), and addressed to an instance it may still command today (the
/// send rule for an Agent). Only a withdrawal reaches another caller's
/// command (the unnarrowed owner's); an edit is always the caller's own.
async fn owned_command(
    state: &AppState,
    device: &Device,
    command_id: &str,
    edit: bool,
) -> Result<Owned, HubError> {
    let row = state
        .store
        .get_command(command_id.to_owned())
        .await?
        .ok_or(HubError::NotFound)?;
    let viewer = viewer(state, device, edit).await?;
    if !visible(&viewer, &row) {
        return Err(HubError::NotFound);
    }
    let instance_id = row.instance_id.clone().ok_or(HubError::NotFound)?;
    let instance = state
        .store
        .get_instance(instance_id.clone())
        .await?
        .ok_or(HubError::NotFound)?;
    if origin(device) == InputOrigin::Agent
        && !owns(state, device, &instance.instance_id).await?
        && !target_in_caller_scope(state, device, &instance).await?
    {
        return Err(HubError::not_owner());
    }
    let node_holds = state
        .store
        .read("node_holds_input", move |conn| {
            node_holds_input(conn, &instance_id)
        })
        .await?;
    let held = held(&row, node_holds);
    Ok(Owned {
        row,
        instance,
        held,
    })
}

/// The `withdrawnBy` record: the caller's origin, and its instance for an
/// Agent. The device id stays in the audit row only.
fn actor(device: &Device) -> Value {
    let origin = crate::agent_scope::origin_wire(device);
    match device.instance_id.as_deref() {
        Some(instance_id) if origin == "agent" => {
            json!({ "origin": origin, "instanceId": instance_id })
        }
        _ => json!({ "origin": origin }),
    }
}

/// An Agent's send is journaled to both transcripts when it is queued
/// (`journal_agent_message`), with the text it had then. After an edit or a
/// withdrawal both transcripts get a text-free note naming the command, so
/// the earlier record is never left standing as what was delivered.
async fn journal_queue_change(state: &AppState, row: &CommandRecord, name: &str, note: &str) {
    let Some(initiator) = &row.initiator else {
        return;
    };
    let mut targets = vec![initiator.instance_id.clone()];
    if let Some(target) = &row.instance_id
        && target != &initiator.instance_id
    {
        targets.push(target.clone());
    }
    for instance_id in targets {
        crate::ws::publish_hub_diagnostic(state, &instance_id, name, note).await;
    }
}

async fn audit(state: &AppState, device: &Device, action: &str, subject: &str, detail: Value) {
    if let Err(error) = state
        .store
        .append_audit(
            device.id.clone(),
            action.to_owned(),
            Some(subject.to_owned()),
            detail,
        )
        .await
    {
        tracing::warn!(%error, action, "command queue audit row not written");
    }
}

// ── edit ───────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditBody {
    expected_revision: Revision,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    attachments: Option<Value>,
}

/// `PATCH /v1/commands/{id}`: replace the text and/or attachments of an own
/// `instance.send` while it is provably undelivered.
async fn edit_command(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(command_id): Path<String>,
    Json(body): Json<EditBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = caller(&state, &headers).await?;
    let expected = body.expected_revision.value("expectedRevision")?;
    if body.text.is_none() && body.attachments.is_none() {
        return Err(HubError::BadRequest(
            "an edit carries `text`, `attachments` or both".into(),
        ));
    }
    let owned = owned_command(&state, &device, &command_id, true).await?;
    if owned.row.operation != "instance.send" {
        return Err(HubError::not_editable(
            "a queued instance.configure cannot be edited (D-055); withdraw it and send a new one",
        ));
    }
    let held = owned
        .held
        .ok_or_else(|| HubError::not_queued(&command_id))?;
    // The text a shell-driver target receives is approved per payload
    // (D-017), so an Agent's new text asks again; the original approval
    // covered the original text only.
    if origin(&device) == InputOrigin::Agent && shell_driver(&owned.instance.driver) {
        require_approval(
            &state,
            &headers,
            &device,
            json!({
                "operation": "command.edit",
                "instanceId": owned.instance.instance_id,
                "commandId": command_id,
                "text": body.text,
                "attachments": body.attachments,
            }),
        )
        .await?;
    }
    let command = match held {
        "hub" => {
            let attachments = match body.attachments {
                Some(attachments) => {
                    // The first POST's own validator, so an edited manifest is
                    // exactly what a fresh send with these objects would store.
                    let mut probe = json!({ "attachments": attachments });
                    crate::objects::validate_send_attachments(
                        &state,
                        &device,
                        &owned.instance,
                        &mut probe,
                    )
                    .await?;
                    Some(probe.get("attachments").cloned().unwrap_or(json!([])))
                }
                None => None,
            };
            let text = body.text.clone();
            let id = command_id.clone();
            state
                .store
                .run_named("edit_queued_command", move |conn| {
                    edit_queued(conn, &id, expected, text.as_deref(), attachments)
                })
                .await
                .map_err(map_store)?
        }
        _ => {
            if body.attachments.is_some() {
                return Err(HubError::not_editable(
                    "the Node already staged this prompt's attachments; withdraw it and send a \
                     new command to change them",
                ));
            }
            let text = body.text.clone().unwrap_or_default();
            edit_on_node(&state, &owned, expected, &text).await?
        }
    };
    journal_queue_change(
        &state,
        &command,
        "queued-command-edited",
        &format!(
            "command {command_id} was edited before delivery (revision {}); only the edited text is sent",
            command.revision
        ),
    )
    .await;
    audit(
        &state,
        &device,
        "command.edit",
        &command_id,
        json!({
            "instanceId": owned.instance.instance_id,
            "revision": command.revision,
            "held": held,
        }),
    )
    .await;
    Ok(Json(json!({ "command": command })))
}

/// Replace the text of a send payload in place, in every shape it carries
/// (`input.text`, a string `input`, `input.blocks`, `text`, `prompt`), so
/// whichever one the Node reads carries the new text. Returns whether any
/// text location existed.
pub(crate) fn replace_send_text(payload: &mut Value, text: &str) -> bool {
    let mut found = false;
    if let Some(object) = payload.as_object_mut() {
        for key in ["text", "prompt"] {
            if object.get(key).is_some_and(Value::is_string) {
                object.insert(key.into(), json!(text));
                found = true;
            }
        }
        if let Some(input) = object.get_mut("input") {
            if input.is_string() {
                *input = json!(text);
                found = true;
            } else if let Some(input) = input.as_object_mut() {
                if input.get("text").is_some_and(Value::is_string) {
                    input.insert("text".into(), json!(text));
                    found = true;
                }
                if let Some(blocks) = input.get_mut("blocks").and_then(Value::as_array_mut)
                    && let Some(first) = blocks
                        .iter()
                        .position(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                {
                    let mut kept = Vec::with_capacity(blocks.len());
                    for (index, block) in blocks.drain(..).enumerate() {
                        let is_text = block.get("type").and_then(Value::as_str) == Some("text");
                        if index == first {
                            kept.push(json!({ "type": "text", "text": text }));
                        } else if !is_text {
                            kept.push(block);
                        }
                    }
                    *blocks = kept;
                    found = true;
                }
            }
        }
    }
    found
}

/// The Hub-held edit, inside the writer job: a CAS on `forwarded = 0` and on
/// the revision. The forward reads the row again after its intent mark, so
/// only the edited text can ever go out.
pub(crate) fn edit_queued(
    conn: &Connection,
    command_id: &str,
    expected_revision: i64,
    text: Option<&str>,
    attachments: Option<Value>,
) -> Result<CommandRecord, StoreError> {
    let row = crate::store::load_command(conn, command_id)?
        .ok_or_else(|| StoreError::NotQueued(command_id.to_owned()))?;
    if row.state != "queued" || row.forwarded {
        return Err(StoreError::NotQueued(command_id.to_owned()));
    }
    if row.revision != expected_revision {
        return Err(StoreError::RevisionConflict {
            command_id: command_id.to_owned(),
            revision: row.revision,
        });
    }
    let mut payload = row.payload.clone();
    if let Some(text) = text
        && !replace_send_text(&mut payload, text)
        && let Some(object) = payload.as_object_mut()
    {
        object.insert("text".into(), json!(text));
    }
    if let Some(attachments) = attachments
        && let Some(object) = payload.as_object_mut()
    {
        if attachments.as_array().is_some_and(Vec::is_empty) {
            object.remove("attachments");
        } else {
            object.insert("attachments".into(), attachments);
        }
    }
    let changed = conn.execute(
        "UPDATE commands
            SET payload_json = ?1, revision = revision + 1,
                original_payload_json = COALESCE(original_payload_json, payload_json),
                updated_at = ?2
          WHERE id = ?3 AND state = 'queued' AND forwarded = 0 AND revision = ?4",
        params![
            payload.to_string(),
            crate::config::now_rfc3339(),
            command_id,
            expected_revision
        ],
    )?;
    if changed == 0 {
        return Err(StoreError::NotQueued(command_id.to_owned()));
    }
    crate::store::load_command(conn, command_id)?
        .ok_or_else(|| StoreError::NotQueued(command_id.to_owned()))
}

/// The Node-held edit: the Node decides against its delivery claim, then the
/// Hub records the text it confirmed.
async fn edit_on_node(
    state: &AppState,
    owned: &Owned,
    expected: i64,
    text: &str,
) -> Result<CommandRecord, HubError> {
    let command_id = owned.row.command_id.clone();
    // The Hub's own revision first; the Node then compares against what it
    // recorded since (it adopts the first `expectedRevision` it is sent).
    if expected != owned.row.revision {
        return Err(HubError::revision_conflict(&command_id, owned.row.revision));
    }
    let answer = state
        .nodes
        .call(
            &owned.row.host_id,
            "command.edit",
            json!({
                "commandId": command_id,
                "instanceId": owned.instance.instance_id,
                "expectedRevision": expected,
                "prompt": text,
            }),
            NODE_CALL_TIMEOUT,
        )
        .await;
    let answer = match answer {
        Ok(Some(frame)) => frame.get("result").cloned().unwrap_or(Value::Null),
        // Offline, or the frame never left: nothing proves it undelivered.
        Ok(None) | Err(_) => return Err(HubError::not_queued(&command_id)),
    };
    match answer.get("outcome").and_then(Value::as_str) {
        Some("edited") => {}
        Some("revision-conflict") => {
            let revision = answer
                .get("revision")
                .and_then(Value::as_i64)
                .unwrap_or(owned.row.revision);
            return Err(HubError::revision_conflict(&command_id, revision));
        }
        _ => return Err(HubError::not_queued(&command_id)),
    }
    let mut payload = owned.row.payload.clone();
    if !replace_send_text(&mut payload, text)
        && let Some(object) = payload.as_object_mut()
    {
        object.insert("text".into(), json!(text));
    }
    let id = command_id.clone();
    let instance_id = owned.instance.instance_id.clone();
    state
        .store
        .run_named("record_node_edit", move |conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            tx.execute(
                "UPDATE commands
                    SET payload_json = ?1, revision = ?2,
                        original_payload_json = COALESCE(original_payload_json, payload_json),
                        updated_at = ?3
                  WHERE id = ?4",
                params![
                    payload.to_string(),
                    expected + 1,
                    crate::config::now_rfc3339(),
                    id
                ],
            )?;
            // D-060: the Node replaced every text block, the "memory changed"
            // line the forward prefixed included, so the next input to the
            // chapter carries it instead.
            release_memory_line(&tx, &instance_id, &id)?;
            let row = crate::store::load_command(&tx, &id)?
                .ok_or_else(|| StoreError::NotQueued(id.clone()))?;
            tx.commit()?;
            Ok(row)
        })
        .await
        .map_err(map_store)
}

// ── withdraw ───────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WithdrawBody {
    #[serde(default)]
    expected_revision: Option<Revision>,
}

/// `POST /v1/commands/{id}/withdraw`: settle an own undelivered command
/// `withdrawn`; it is never forwarded. Withdrawing it again returns the row.
async fn withdraw_command(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(command_id): Path<String>,
    body: Option<Json<WithdrawBody>>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = caller(&state, &headers).await?;
    let expected = match body
        .as_ref()
        .and_then(|body| body.expected_revision.as_ref())
    {
        Some(revision) => Some(revision.value("expectedRevision")?),
        None => None,
    };
    let owned = owned_command(&state, &device, &command_id, false).await?;
    if owned.row.settlement_outcome.as_deref() == Some("withdrawn") {
        return Ok(Json(json!({ "command": owned.row })));
    }
    let held = owned
        .held
        .ok_or_else(|| HubError::not_queued(&command_id))?;
    let by = actor(&device);
    let command = match held {
        "hub" => {
            let id = command_id.clone();
            let by = by.clone();
            state
                .store
                .run_named("withdraw_queued_command", move |conn| {
                    withdraw_queued(conn, &id, expected, &by)
                })
                .await
                .map_err(map_store)?
        }
        _ => withdraw_on_node(&state, &owned, expected, &by).await?,
    };
    journal_queue_change(
        &state,
        &command,
        "queued-command-withdrawn",
        &format!("command {command_id} was withdrawn before delivery; it was never sent"),
    )
    .await;
    audit(
        &state,
        &device,
        "command.withdraw",
        &command_id,
        json!({
            "instanceId": owned.instance.instance_id,
            "revision": command.revision,
            "held": held,
        }),
    )
    .await;
    Ok(Json(json!({ "command": command })))
}

/// The Hub-held withdraw, inside the writer job. The forward-intent mark
/// runs in the same writer and only marks a row still `queued`, so exactly
/// one of withdraw and forward wins.
pub(crate) fn withdraw_queued(
    conn: &mut Connection,
    command_id: &str,
    expected_revision: Option<i64>,
    by: &Value,
) -> Result<CommandRecord, StoreError> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let row = crate::store::load_command(&tx, command_id)?
        .ok_or_else(|| StoreError::NotQueued(command_id.to_owned()))?;
    if row.settlement_outcome.as_deref() == Some("withdrawn") {
        return Ok(row);
    }
    if row.state != "queued" || row.forwarded {
        return Err(StoreError::NotQueued(command_id.to_owned()));
    }
    if let Some(expected) = expected_revision
        && expected != row.revision
    {
        return Err(StoreError::RevisionConflict {
            command_id: command_id.to_owned(),
            revision: row.revision,
        });
    }
    let now = crate::config::now_rfc3339();
    let changed = tx.execute(
        "UPDATE commands
            SET state = 'settled', resolution = 'clear', settlement_outcome = 'withdrawn',
                settlement_reason = NULL, withdrawn_by_json = ?1, updated_at = ?2
          WHERE id = ?3 AND state = 'queued' AND forwarded = 0",
        params![by.to_string(), now, command_id],
    )?;
    if changed == 0 {
        return Err(StoreError::NotQueued(command_id.to_owned()));
    }
    if let Some(instance_id) = row.instance_id.as_deref() {
        // D-060: a withdrawn send never carried its "memory changed" line, so
        // the next input to the chapter carries it instead.
        release_memory_line(&tx, instance_id, command_id)?;
        if row.operation == "instance.configure" {
            restore_configure(&tx, instance_id, command_id, &now)?;
        }
    }
    let row = crate::store::load_command(&tx, command_id)?
        .ok_or_else(|| StoreError::NotQueued(command_id.to_owned()))?;
    tx.commit()?;
    Ok(row)
}

fn release_memory_line(
    conn: &Connection,
    instance_id: &str,
    command_id: &str,
) -> Result<(), StoreError> {
    let rev: Option<i64> = conn
        .query_row(
            "SELECT memory_change_rev FROM commands WHERE id = ?1",
            params![command_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    if let Some(rev) = rev {
        conn.execute(
            "UPDATE instances SET memory_seen_rev = ?2 - 1
              WHERE id = ?1 AND memory_seen_rev = ?2",
            params![instance_id, rev],
        )?;
        conn.execute(
            "UPDATE commands SET memory_change_rev = NULL WHERE id = ?1",
            params![command_id],
        )?;
    }
    Ok(())
}

/// Restore every field a withdrawn configure set that still holds the value
/// it set; a restore is itself a config change and bumps the generation.
fn restore_configure(
    conn: &Connection,
    instance_id: &str,
    command_id: &str,
    now: &str,
) -> Result<(), StoreError> {
    let prior: Option<String> = conn
        .query_row(
            "SELECT config_prior_json FROM commands WHERE id = ?1",
            params![command_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let Some(prior) = prior.and_then(|raw| serde_json::from_str::<Value>(&raw).ok()) else {
        return Ok(());
    };
    let spec_raw: Option<String> = conn
        .query_row(
            "SELECT spec_json FROM instances WHERE id = ?1",
            params![instance_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(mut spec) = spec_raw.and_then(|raw| serde_json::from_str::<Value>(&raw).ok()) else {
        return Ok(());
    };
    let (Some(set), Some(before)) = (
        prior.get("set").and_then(Value::as_object),
        prior.get("prior").and_then(Value::as_object),
    ) else {
        return Ok(());
    };
    let Some(object) = spec.as_object_mut() else {
        return Ok(());
    };
    let mut restored = false;
    for (key, value) in set {
        if object.get(key) != Some(value) {
            continue;
        }
        match before.get(key) {
            Some(Value::Null) | None => {
                object.remove(key);
            }
            Some(previous) => {
                object.insert(key.clone(), previous.clone());
            }
        }
        restored = true;
    }
    if restored {
        conn.execute(
            "UPDATE instances
                SET spec_json = ?1, updated_at = ?2, configure_seq = configure_seq + 1
              WHERE id = ?3",
            params![spec.to_string(), now, instance_id],
        )?;
    }
    Ok(())
}

/// The Node-held withdraw: the Node's claim decides; the Hub then settles its
/// accepted row `withdrawn`.
async fn withdraw_on_node(
    state: &AppState,
    owned: &Owned,
    expected: Option<i64>,
    by: &Value,
) -> Result<CommandRecord, HubError> {
    let command_id = owned.row.command_id.clone();
    if let Some(expected) = expected
        && expected != owned.row.revision
    {
        return Err(HubError::revision_conflict(&command_id, owned.row.revision));
    }
    let answer = state
        .nodes
        .call(
            &owned.row.host_id,
            "command.withdraw",
            json!({
                "commandId": command_id,
                "instanceId": owned.instance.instance_id,
            }),
            NODE_CALL_TIMEOUT,
        )
        .await;
    match answer {
        Ok(Some(frame))
            if frame.pointer("/result/outcome").and_then(Value::as_str) == Some("withdrawn") => {}
        _ => return Err(HubError::not_queued(&command_id)),
    }
    let id = command_id.clone();
    let instance_id = owned.instance.instance_id.clone();
    let by = by.to_string();
    state
        .store
        .run_named("record_node_withdraw", move |conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            // The Node's own `cancelled` settlement may land first through the
            // journal; the withdrawal is still what happened.
            let changed = tx.execute(
                "UPDATE commands
                    SET state = 'settled', resolution = 'clear',
                        settlement_outcome = 'withdrawn', settlement_reason = NULL,
                        withdrawn_by_json = ?1, updated_at = ?2
                  WHERE id = ?3 AND (state <> 'settled'
                        OR (settlement_outcome = 'cancelled'
                            AND COALESCE(settlement_reason, 'withdrawn') = 'withdrawn'))",
                params![by, crate::config::now_rfc3339(), id],
            )?;
            if changed > 0 {
                // D-060: as for a Hub-held withdrawal, the line never reached
                // the chapter.
                release_memory_line(&tx, &instance_id, &id)?;
            }
            let row = crate::store::load_command(&tx, &id)?
                .ok_or_else(|| StoreError::NotQueued(id.clone()))?;
            tx.commit()?;
            Ok(row)
        })
        .await
        .map_err(map_store)
}

// ── runtime config and its generation ──────────────────────────────────────

/// `GET /v1/instances/{id}/config`.
async fn get_config(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(instance_id): Path<String>,
) -> Result<Json<Value>, HubError> {
    require_instance_read(&state, &headers, &instance_id).await?;
    let view = state
        .store
        .read("instance_config_view", move |conn| {
            config_view(conn, &instance_id)
        })
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    Ok(Json(view))
}

/// The instance's runtime config: the requested values, what the transcript
/// shows in effect, the generation and the changes not yet in effect.
pub(crate) fn config_view(
    conn: &Connection,
    instance_id: &str,
) -> Result<Option<Value>, StoreError> {
    let row: Option<(String, i64)> = conn
        .query_row(
            "SELECT spec_json, configure_seq FROM instances WHERE id = ?1",
            params![instance_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((spec_raw, generation)) = row else {
        return Ok(None);
    };
    let spec: Value = serde_json::from_str(&spec_raw).unwrap_or(json!({}));
    let field = |key: &str| spec.get(key).cloned().unwrap_or(Value::Null);
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM commands c
          WHERE c.instance_id = ?1 AND c.operation = 'instance.configure'
            AND ((c.state = 'queued' AND c.forwarded = 0)
                 OR (c.config_deferred IS NOT NULL AND c.config_deferred_resolved_at IS NULL
                     AND COALESCE(c.settlement_outcome, 'completed') = 'completed'))
          ORDER BY c.created_at DESC, c.id DESC
          LIMIT 20"
    ))?;
    let pending = stmt
        .query_map(params![instance_id], crate::store::command_from_row)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|row| pending_item(&row))
        .collect::<Vec<_>>();
    Ok(Some(json!({
        "instanceId": instance_id,
        "generation": generation,
        "model": field("model"),
        "effort": field("effort"),
        "permissionMode": field("permissionMode"),
        "effective": {
            "model": field("modelEffective"),
            "effort": field("effortEffective"),
        },
        "pending": pending,
    })))
}

fn pending_item(row: &CommandRecord) -> Value {
    let boundary = row
        .deferred
        .as_ref()
        .and_then(|deferred| deferred.get("boundary"))
        .cloned();
    let state = if row.state == "queued" && !row.forwarded {
        "queued"
    } else {
        "deferred"
    };
    let mut item = json!({
        "commandId": row.command_id,
        "state": state,
        "createdAt": row.created_at,
    });
    if let Some(object) = item.as_object_mut() {
        if let Some(boundary) = boundary {
            object.insert("boundary".into(), boundary);
        }
        for key in ["model", "effort", "permissionMode"] {
            if let Some(value) = row.payload.get(key) {
                object.insert(key.into(), value.clone());
            }
        }
        if let Some(initiator) = &row.initiator {
            object.insert(
                "initiator".into(),
                serde_json::to_value(initiator).unwrap_or(Value::Null),
            );
        }
    }
    item
}

/// The newest configure of an instance that is not in effect yet, for the
/// worker digest: `(state, requestId, boundary)` with state `queued`,
/// `accepted` or `deferred`.
pub(crate) fn pending_configure(
    commands: &[CommandRecord],
) -> Option<(String, Option<String>, Option<String>)> {
    commands
        .iter()
        .filter(|command| command.operation == "instance.configure")
        .find_map(|command| {
            // Its own applied (or failed) edge was journaled: in effect, or
            // never going to be, either way no longer pending.
            if command
                .deferred
                .as_ref()
                .is_some_and(|deferred| deferred.get("resolvedAt").is_some())
            {
                return None;
            }
            let boundary = command
                .deferred
                .as_ref()
                .filter(|deferred| deferred.get("resolvedAt").is_none())
                .and_then(|deferred| deferred.get("boundary"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let open = command.state != "settled";
            let deferred = boundary.is_some()
                && command
                    .settlement_outcome
                    .as_deref()
                    .is_none_or(|outcome| outcome == "completed");
            if !open && !deferred {
                return None;
            }
            let state = if command.state == "queued" && !command.forwarded {
                "queued".to_owned()
            } else if deferred {
                "deferred".to_owned()
            } else {
                command.state.clone()
            };
            let request_id = command
                .payload
                .pointer("/effort/requestId")
                .or_else(|| command.payload.get("requestId"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string);
            Some((state, request_id, boundary))
        })
}

/// Whether a configure payload changes effort.
pub(crate) fn changes_effort(payload: &Value) -> bool {
    EFFORT_KEYS
        .iter()
        .any(|key| payload.get(*key).is_some_and(|value| !value.is_null()))
}

/// Whether a configure payload changes the model.
pub(crate) fn changes_model(payload: &Value) -> bool {
    payload
        .get("model")
        .or_else(|| payload.get("modelId"))
        .and_then(Value::as_str)
        .is_some_and(|model| !model.is_empty())
}

/// Remove the effort keys from a forwarded configure frame whose carrier has
/// no in-session effort channel (the spec merge already holds the change).
pub(crate) fn strip_effort(params: &mut serde_json::Map<String, Value>) {
    for key in EFFORT_KEYS {
        params.remove(*key);
    }
}

/// `next-chapter`: a configure that changes effort on a carrier that only
/// takes effort at launch. When it changes nothing else the Hub settles it
/// `completed` without a Node round trip; otherwise the deferral is recorded
/// and the forward drops the effort keys. Returns the row.
pub(crate) fn defer_to_next_chapter(
    conn: &Connection,
    command_id: &str,
    settle: bool,
) -> Result<Option<CommandRecord>, StoreError> {
    if settle {
        conn.execute(
            "UPDATE commands
                SET state = 'settled', resolution = 'clear', settlement_outcome = 'completed',
                    settlement_reason = NULL, config_deferred = ?1, updated_at = ?2
              WHERE id = ?3 AND state = 'queued' AND forwarded = 0",
            params![NEXT_CHAPTER, crate::config::now_rfc3339(), command_id],
        )?;
    } else {
        conn.execute(
            "UPDATE commands SET config_deferred = ?1 WHERE id = ?2",
            params![NEXT_CHAPTER, command_id],
        )?;
    }
    crate::store::load_command(conn, command_id)
}

/// `next-turn`: a forwarded configure to a busy PTY carrier waits for the
/// next idle moment. A switch already observed applied (its read-back raced
/// ahead of this) stays resolved.
pub(crate) fn defer_to_next_turn(
    conn: &Connection,
    command_id: &str,
) -> Result<Option<CommandRecord>, StoreError> {
    conn.execute(
        "UPDATE commands SET config_deferred = ?1
          WHERE id = ?2 AND config_deferred IS NULL AND config_deferred_resolved_at IS NULL",
        params![NEXT_TURN, command_id],
    )?;
    crate::store::load_command(conn, command_id)
}

/// Whether a configure to `instance` that changes these axes waits for a turn
/// boundary on its carrier: a PTY carrier switches effort and model only at
/// an idle moment.
pub(crate) fn waits_for_turn(instance: &InstanceRecord, payload: &Value) -> bool {
    HELD_INPUT_DRIVERS.contains(&instance.driver.as_str())
        && (changes_effort(payload) || changes_model(payload))
        && matches!(
            instance.activity.as_str(),
            "working" | "blocked" | "draining"
        )
}

/// Fold one journaled `instance.configure` lifecycle into the configure
/// rows' deferral: `effort-queued` (re)marks the request deferred until the
/// next turn; a terminal effort edge (`applied`, `degraded`,
/// `unsupported-in-session`, `control-unavailable`) resolves it. Matched by
/// the request's own `effortRequestId`; an edge without one changes nothing.
pub(crate) fn fold_configure_lifecycle(
    conn: &Connection,
    instance_id: &str,
    payload: &Value,
) -> Result<(), StoreError> {
    if payload.get("type").and_then(Value::as_str) != Some("native")
        || payload.get("nativeName").and_then(Value::as_str) != Some("instance.configure")
    {
        return Ok(());
    }
    let Some(status) = payload.get("status").and_then(Value::as_str) else {
        return Ok(());
    };
    let Some(request_id) = payload
        .pointer(&format!(
            "/relatedIds/{}",
            remuda_protocol::EFFORT_REQUEST_ID_KEY
        ))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
    else {
        return Ok(());
    };
    let now = crate::config::now_rfc3339();
    if status.starts_with("effort-queued:") {
        conn.execute(
            "UPDATE commands SET config_deferred = ?1
              WHERE instance_id = ?2 AND operation = 'instance.configure'
                AND json_extract(payload_json, '$.effort.requestId') = ?3
                AND config_deferred_resolved_at IS NULL
                AND COALESCE(config_deferred, ?1) = ?1",
            params![NEXT_TURN, instance_id, request_id],
        )?;
    } else if [
        "effort-applied:",
        "effort-degraded:",
        "effort-unsupported-in-session:",
        "effort-control-unavailable:",
    ]
    .iter()
    .any(|prefix| status.starts_with(prefix))
    {
        conn.execute(
            "UPDATE commands SET config_deferred_resolved_at = ?1
              WHERE instance_id = ?2 AND operation = 'instance.configure'
                AND json_extract(payload_json, '$.effort.requestId') = ?3
                AND config_deferred_resolved_at IS NULL
                AND COALESCE(config_deferred, ?4) = ?4",
            params![now, instance_id, request_id, NEXT_TURN],
        )?;
    }
    Ok(())
}

/// Record a configure's merge for a later withdrawal: the values it replaced
/// and the ones it set, inside the merge's writer job.
pub(crate) fn record_configure_prior(
    conn: &Connection,
    command_id: &str,
    prior: &serde_json::Map<String, Value>,
    set: &serde_json::Map<String, Value>,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE commands SET config_prior_json = ?1 WHERE id = ?2",
        params![
            json!({ "prior": prior, "set": set }).to_string(),
            command_id
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_send_text_rewrites_every_text_location_and_keeps_media() {
        let mut payload = json!({
            "prompt": "old",
            "input": {
                "type": "prompt",
                "blocks": [
                    { "type": "image", "objectId": "obj_1" },
                    { "type": "text", "text": "old a" },
                    { "type": "text", "text": "old b" },
                ],
            },
        });
        assert!(replace_send_text(&mut payload, "new"));
        assert_eq!(payload["prompt"], "new");
        assert_eq!(
            payload["input"]["blocks"],
            json!([
                { "type": "image", "objectId": "obj_1" },
                { "type": "text", "text": "new" },
            ])
        );
        let mut plain = json!({ "input": { "text": "x" } });
        assert!(replace_send_text(&mut plain, "y"));
        assert_eq!(plain["input"]["text"], "y");
        let mut bare = json!({ "input": "x" });
        assert!(replace_send_text(&mut bare, "y"));
        assert_eq!(bare["input"], "y");
        let mut none = json!({ "mode": "queue" });
        assert!(!replace_send_text(&mut none, "y"));
    }

    fn row(operation: &str, state: &str, forwarded: bool, payload: Value) -> CommandRecord {
        serde_json::from_value(json!({
            "commandId": "cmd_x", "instanceId": "ins_x", "hostId": "hst_x",
            "operation": operation, "state": state, "resolution": "clear",
            "forwarded": forwarded, "payload": payload, "idempotencyKey": null,
            "createdAt": "t", "updatedAt": "t",
        }))
        .unwrap()
    }

    #[test]
    fn visibility_follows_the_stamp_and_never_reaches_hub_input() {
        let agent = Viewer::Agent {
            instance_id: "ins_main".into(),
            lineage_id: "ins_main".into(),
        };
        let mut agents = row("instance.send", "queued", false, json!({"origin": "agent"}));
        agents.initiator = Some(remuda_protocol::Initiator {
            instance_id: "ins_other".into(),
            lineage_id: "ins_main".into(),
            generation: 2,
            root_turn: None,
        });
        assert!(visible(&agent, &agents), "same lineage, earlier chapter");
        let other = Viewer::Agent {
            instance_id: "ins_b".into(),
            lineage_id: "ins_b".into(),
        };
        assert!(!visible(&other, &agents), "another Agent's command");
        let mut humans = row("instance.send", "queued", false, json!({"origin": "human"}));
        humans.queued_by_device_id = Some("dev_phone".into());
        assert!(!visible(&agent, &humans), "an Agent never sees a Human's");
        assert!(visible(&Viewer::Device("dev_phone".into()), &humans));
        assert!(!visible(&Viewer::Device("dev_laptop".into()), &humans));
        assert!(visible(&Viewer::Owner, &humans));
        assert!(visible(&Viewer::Owner, &agents));
        let hub = row("instance.send", "queued", false, json!({"origin": "hub"}));
        assert!(
            !visible(&Viewer::Owner, &hub),
            "Hub-written input is nobody's"
        );
        let close = row(
            "instance.close",
            "queued",
            false,
            json!({"origin": "human"}),
        );
        assert!(!visible(&Viewer::Owner, &close));
    }

    #[test]
    fn held_is_only_provable_undelivery() {
        let queued = row("instance.send", "queued", false, json!({}));
        assert_eq!(held(&queued, false), Some("hub"));
        let in_flight = row("instance.send", "queued", true, json!({}));
        assert_eq!(
            held(&in_flight, true),
            None,
            "an unknown forward is not queued"
        );
        let accepted = row("instance.send", "accepted", true, json!({}));
        assert_eq!(held(&accepted, true), Some("node"));
        assert_eq!(held(&accepted, false), None, "no heldInput: never guessed");
        let configure = row("instance.configure", "accepted", true, json!({}));
        assert_eq!(held(&configure, true), None);
        let settled = row("instance.send", "settled", false, json!({}));
        assert_eq!(held(&settled, true), None);
    }
}
