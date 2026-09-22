//! HTTP surface for pending Interactions and first-answer-wins respond.

use crate::AppState;
use crate::auth::require_origin;
use crate::error::HubError;
use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use remuda_protocol::{CommandId, Id, InteractionAnswer, InteractionId};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

const NODE_RPC_TIMEOUT: Duration = Duration::from_secs(5);

/// Interaction kind that must never route to an agent (D-051 (2), D-017).
const APPROVAL_KIND: &str = "approval";
/// Session posture that disables every downstream gate (D-011).
const BYPASS_PERMISSIONS: &str = "bypassPermissions";

// ---------------------------------------------------------------------------
// D-051 feature switch: `REMUDA_DELEGATED_DECISIONS`, per-project override
// winning over the global switch (D-011 convention).
//
// The global switch is the `REMUDA_DELEGATED_DECISIONS` environment variable
// (truthy: `1`/`true`/`on`/`yes`/`enabled`; absent or anything else = off).
// Two comma-separated project-id lists override it per project:
// `REMUDA_DELEGATED_DECISIONS_FORCE_ON` and `..._FORCE_OFF`. A project on the
// OFF list is refused even when the global switch is on; a project on the ON
// list is admitted when the global switch is off. Appearing on both lists
// fails closed (OFF wins). No wire/schema field carries this: it is an
// operator rollout switch, not an agent-facing policy.
// ---------------------------------------------------------------------------

const DELEGATED_DECISIONS_ENV: &str = "REMUDA_DELEGATED_DECISIONS";
const DELEGATED_DECISIONS_FORCE_ON_ENV: &str = "REMUDA_DELEGATED_DECISIONS_FORCE_ON";
const DELEGATED_DECISIONS_FORCE_OFF_ENV: &str = "REMUDA_DELEGATED_DECISIONS_FORCE_OFF";

static DELEGATED_GLOBAL_OVERRIDE: std::sync::RwLock<Option<bool>> = std::sync::RwLock::new(None);

fn env_bool(name: &str) -> Option<bool> {
    let raw = std::env::var(name).ok()?;
    Some(matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "on" | "yes" | "enabled"
    ))
}

fn env_id_list(name: &str) -> Vec<String> {
    std::env::var(name)
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Pure resolution of the D-051 switch. `None` project means "outside any
/// project"; only the global switch applies.
fn resolve_delegated_decisions(
    global: bool,
    force_on: &[String],
    force_off: &[String],
    project_id: Option<&str>,
) -> bool {
    if let Some(project_id) = project_id {
        // Fail closed when an id appears on both lists.
        if force_off.iter().any(|id| id == project_id) {
            return false;
        }
        if force_on.iter().any(|id| id == project_id) {
            return true;
        }
    }
    global
}

/// Whether the D-051 relaxations are live for the given instance's project.
/// Defaults to off, so an unconfigured Hub behaves byte-for-byte as before.
pub(crate) fn delegated_decisions_enabled(project_id: Option<&str>) -> bool {
    let global = DELEGATED_GLOBAL_OVERRIDE
        .read()
        .map(|guard| *guard)
        .unwrap_or(None)
        .or_else(|| env_bool(DELEGATED_DECISIONS_ENV))
        .unwrap_or(false);
    resolve_delegated_decisions(
        global,
        &env_id_list(DELEGATED_DECISIONS_FORCE_ON_ENV),
        &env_id_list(DELEGATED_DECISIONS_FORCE_OFF_ENV),
        project_id,
    )
}

/// Hub-side facts about the instance owning an Interaction, gathered the same
/// way for the list filter and the answer gate. `parent_instance_id` is the
/// Hub-stamped create-time edge read by `owns()`
/// (`crates/remuda-hub/src/agent_scope.rs:63-71`); `permissionMode` comes
/// from the raw create spec via the existing accessor
/// (`crates/remuda-hub/src/store.rs:3551` `get_instance_spec_json`).
struct RouteTarget {
    parent_instance_id: Option<String>,
    project_id: Option<String>,
    permission_mode: Option<String>,
}

impl RouteTarget {
    async fn load(state: &AppState, instance_id: &str) -> Result<Option<Self>, HubError> {
        let Some(instance) = state.store.get_instance(instance_id.to_string()).await? else {
            return Ok(None);
        };
        let permission_mode = state
            .store
            .get_instance_spec_json(instance_id.to_string())
            .await?
            .and_then(|spec| {
                spec.get("permissionMode")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        Ok(Some(Self {
            parent_instance_id: instance.parent_instance_id,
            project_id: instance.project_id,
            permission_mode,
        }))
    }
}

/// Filter a *merged* interaction page (durable SQL rows, in-memory
/// `agent_approvals` views, live Node RPC items) down to what the Agent
/// caller may see under the D-051 one-hop rule. Filtering happens after the
/// branches merge so an in-memory approval grant cannot bypass the kind
/// exclusion via a different branch.
async fn delegated_visible_items(
    state: &AppState,
    device: &crate::store::Device,
    items: Vec<Value>,
) -> Result<Vec<Value>, HubError> {
    let Some(caller_instance) = device.instance_id.as_deref() else {
        return Ok(Vec::new());
    };
    // Parent-posture exclusion (D-051 (b)): a caller holding a human-granted
    // bypass mandate must not route anything, even to manual-posture children.
    let caller_mode = RouteTarget::load(state, caller_instance).await?;
    if caller_mode
        .and_then(|target| target.permission_mode)
        .as_deref()
        == Some(BYPASS_PERMISSIONS)
    {
        return Ok(Vec::new());
    }
    let mut resolved: HashMap<String, Option<RouteTarget>> = HashMap::new();
    let mut kept = Vec::with_capacity(items.len());
    for item in items {
        let Some(target) = item.get("instanceId").and_then(Value::as_str) else {
            continue;
        };
        // (a) approvals never route, from whichever branch the item came.
        if item.get("kind").and_then(Value::as_str) == Some(APPROVAL_KIND) {
            continue;
        }
        if !resolved.contains_key(target) {
            resolved.insert(target.to_string(), RouteTarget::load(state, target).await?);
        }
        let Some(route_target) = resolved.get(target).unwrap() else {
            // Missing instance: `owns()` fails the same way.
            continue;
        };
        // The single one-hop family edge: self or direct child.
        if target != caller_instance
            && route_target.parent_instance_id.as_deref() != Some(caller_instance)
        {
            continue;
        }
        // (b) bypass-posture child never routes.
        if route_target.permission_mode.as_deref() == Some(BYPASS_PERMISSIONS) {
            continue;
        }
        if !delegated_decisions_enabled(route_target.project_id.as_deref()) {
            continue;
        }
        kept.push(item);
    }
    Ok(kept)
}

/// Agent admission for `POST /v1/interactions/{id}/answer`. Returns the stored
/// interaction when admitted; every other case is `Forbidden`, including an
/// unknown id (an agent must not learn whether an interaction exists).
async fn authorize_agent_answer(
    state: &AppState,
    device: &crate::store::Device,
    interaction_id: &InteractionId,
) -> Result<crate::store::InteractionRecord, HubError> {
    let row = state
        .store
        .get_interaction(interaction_id.as_id().to_string())
        .await?
        .ok_or(HubError::Forbidden)?;
    // (a) Approvals never route: answering one hands the authorization itself
    // to a caller that may not hold it (D-017 confused deputy). The in-memory
    // one-shot approvals additionally carry their own origin gate in
    // `crates/remuda-hub/src/agent_approvals.rs:234` `answer`.
    if row.kind == APPROVAL_KIND {
        return Err(HubError::Forbidden);
    }
    // The handler re-checks the same one-hop edge the middleware relies on:
    // self, or target's `parent_instance_id == caller`
    // (`crates/remuda-hub/src/agent_scope.rs:63-71`).
    if !crate::agent_scope::owns(state, device, &row.instance_id).await? {
        return Err(HubError::Forbidden);
    }
    let route_target = RouteTarget::load(state, &row.instance_id)
        .await?
        .ok_or(HubError::Forbidden)?;
    if !delegated_decisions_enabled(route_target.project_id.as_deref()) {
        return Err(HubError::Forbidden);
    }
    // (b) Bypass posture on EITHER side refuses (D-011): the child has no
    // downstream gates, and a bypass-mandated parent must not lend its mandate
    // to the child's next action. Both reads go through the same existing
    // `get_instance_spec_json` accessor (`store.rs:3551`), one for each side.
    if route_target.permission_mode.as_deref() == Some(BYPASS_PERMISSIONS) {
        return Err(HubError::Forbidden);
    }
    let caller_instance = device.instance_id.as_deref().ok_or(HubError::Forbidden)?;
    let caller_mode = RouteTarget::load(state, caller_instance)
        .await?
        .and_then(|target| target.permission_mode);
    if caller_mode.as_deref() == Some(BYPASS_PERMISSIONS) {
        return Err(HubError::Forbidden);
    }
    Ok(row)
}

/// Context for an accepted Bot-relayed answer (design §5.1 #1).
struct BotRelay {
    /// Acting owner Feishu `open_id`.
    open_id: String,
    /// Open card ticket that authorized the relay.
    ticket_id: String,
}

/// REST routes for `/v1/interactions`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/interactions", get(list_interactions))
        .route("/v1/interactions/{id}/answer", post(answer_interaction))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListQuery {
    #[serde(default)]
    host_id: Option<String>,
    #[serde(default)]
    instance_id: Option<String>,
    #[serde(default)]
    kind: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnswerBody {
    #[serde(default)]
    command_id: Option<String>,
    answer: InteractionAnswer,
    /// Feishu `open_id` of the owner who acted. Required, and only consulted,
    /// when the caller is a Bot device relaying a card click (design §5.1 #1).
    #[serde(default)]
    acting_open_id: Option<String>,
}

/// `GET /v1/interactions` — durable Hub index, merged with live Node RPC.
pub async fn list_interactions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, HubError> {
    let device = crate::agent_scope::caller(&state, &headers).await?;
    let agent_origin = crate::agent_scope::origin(&device) == remuda_protocol::InputOrigin::Agent;
    // D-051: an Agent reaches the merged interaction list only while the
    // switch is live for the addressed project. With the switch off the
    // refusal is the blanket operator-only one, byte-for-byte as before.
    if agent_origin {
        let admitted = match query.instance_id.as_deref() {
            // Pinned read: the one-hop `owns()` edge plus THAT instance's
            // project switch. A miss is a refusal, not an empty page.
            Some(target) => {
                if !crate::agent_scope::owns(&state, &device, target).await? {
                    return Err(HubError::Forbidden);
                }
                let project_id = state
                    .store
                    .get_instance(target.to_string())
                    .await?
                    .and_then(|instance| instance.project_id);
                delegated_decisions_enabled(project_id.as_deref())
            }
            // Unpinned read: the global switch, or an explicit per-project
            // roll-out list. The merged-page filter below stays authoritative
            // per row (off-list/foreign projects produce nothing regardless).
            None => {
                delegated_decisions_enabled(None)
                    || !env_id_list(DELEGATED_DECISIONS_FORCE_ON_ENV).is_empty()
            }
        };
        if !admitted {
            return Err(HubError::Forbidden);
        }
    }
    let mut items: Vec<Value> = state
        .store
        .list_interactions(
            query.host_id.clone(),
            query.instance_id.clone(),
            query.kind.clone(),
            true,
        )
        .await?
        .into_iter()
        .map(|row| row.to_list_item())
        .collect();
    items.extend(
        state
            .agent_approvals
            .list(&device)
            .await
            .into_iter()
            .filter(|item| {
                query
                    .host_id
                    .as_deref()
                    .is_none_or(|host| item["hostId"] == host)
                    && query
                        .instance_id
                        .as_deref()
                        .is_none_or(|id| item["instanceId"] == id)
                    && query.kind.as_deref().is_none_or(|kind| kind == "approval")
            }),
    );
    let mut seen: HashSet<String> = items
        .iter()
        .filter_map(|item| {
            item.get("interactionId")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    let mut params = json!({});
    if let Some(instance_id) = &query.instance_id
        && let Some(obj) = params.as_object_mut()
    {
        obj.insert("instanceId".into(), json!(instance_id));
    }
    if let Some(kind) = &query.kind
        && let Some(obj) = params.as_object_mut()
    {
        obj.insert("kind".into(), json!(kind));
    }
    let hosts = match &query.host_id {
        Some(host_id) => vec![host_id.clone()],
        None => state.nodes.host_ids().await,
    };
    for host_id in hosts {
        match state
            .nodes
            .call(
                &host_id,
                "interaction.list",
                params.clone(),
                NODE_RPC_TIMEOUT,
            )
            .await
        {
            Ok(Some(frame)) => match rpc_result(frame) {
                Ok(result) => {
                    if let Some(batch) = result.get("items").and_then(Value::as_array) {
                        for item in batch {
                            if query.kind.as_ref().is_none_or(|want| {
                                item.get("kind").and_then(Value::as_str) == Some(want.as_str())
                            }) {
                                let id = item
                                    .get("interactionId")
                                    .or_else(|| item.get("id"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                if id.is_empty() || seen.insert(id.to_string()) {
                                    items.push(flatten_interaction(item.clone()));
                                }
                            }
                        }
                    }
                }
                Err(HubError::NotFound) => {}
                Err(err) => tracing::debug!(error = %err, %host_id, "interaction.list"),
            },
            Ok(None) => {}
            Err(err) => tracing::debug!(error = %err, %host_id, "interaction.list rpc"),
        }
    }
    // D-051: the Agent relaxation is applied to the fully merged page, so the
    // approval-kind exclusion cannot be bypassed via the in-memory grant
    // branch (merged above from `agent_approvals.list`,
    // `crates/remuda-hub/src/agent_approvals.rs:217`). Operators keep the
    // unfiltered page exactly as before.
    if agent_origin {
        items = delegated_visible_items(&state, &device, items).await?;
    }
    Ok(Json(json!({ "items": items, "nextCursor": null })))
}

/// `POST /v1/interactions/:id/answer` — unique commandId wins.
pub async fn answer_interaction(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<AnswerBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = crate::agent_scope::caller(&state, &headers).await?;
    let origin = crate::agent_scope::origin(&device);
    let interaction_id =
        InteractionId::try_from(id).map_err(|err| HubError::BadRequest(err.to_string()))?;
    // D-051: the blanket Agent refusal that used to sit here
    // (`if origin == Agent { return Err(Forbidden) }`) is relaxed exactly for
    // the one-hop `owns()` edge, with the approval/bypass/switch exclusions
    // enforced mechanically in `authorize_agent_answer` below. An admitted row
    // is reused later instead of re-reading it.
    let preloaded = if origin == remuda_protocol::InputOrigin::Agent {
        Some(authorize_agent_answer(&state, &device, &interaction_id).await?)
    } else {
        None
    };
    // D-005/D-011 stay in force for Bot callers (design §5.1 #1): the bot is
    // only the owner's courier. A relay is accepted exactly when all hold:
    //   1. the answer carries the acting owner's Feishu open_id,
    //   2. that open_id is on this bot device's allowlist,
    //   3. an open, unexpired card ticket binds this interaction to the bot.
    // Agent-relayed approvals without these stay untrusted and get 403.
    let bot_relay = if origin == remuda_protocol::InputOrigin::Bot {
        let acting_open_id = body
            .acting_open_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
            .ok_or(HubError::Forbidden)?;
        if !state
            .store
            .bot_owner_allowlist_contains(device.id.clone(), acting_open_id.to_string())
            .await?
        {
            return Err(HubError::Forbidden);
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        let ticket = state
            .store
            .get_open_card_ticket_for_bot(
                interaction_id.as_id().as_str().to_string(),
                device.id.clone(),
            )
            .await?
            .ok_or(HubError::Forbidden)?;
        if ticket.expires_at_ms <= now_ms {
            return Err(HubError::Forbidden);
        }
        Some(BotRelay {
            open_id: acting_open_id.to_string(),
            ticket_id: ticket.ticket_id,
        })
    } else {
        None
    };
    let command_id = match body.command_id {
        Some(raw) => {
            CommandId::try_from(raw).map_err(|err| HubError::BadRequest(err.to_string()))?
        }
        None => CommandId::new(),
    };
    let device_id = device.id.clone();
    let by_device =
        Id::try_from(device.id.clone()).map_err(|err| HubError::Internal(err.to_string()))?;
    if let Some(result) = state
        .agent_approvals
        .answer(
            &interaction_id,
            body.answer.clone(),
            &device,
            command_id.clone(),
        )
        .await?
    {
        if let Some(relay) = &bot_relay {
            settle_bot_relay(
                &state,
                &device_id,
                relay,
                interaction_id.as_id().as_str(),
                command_id.as_id().as_str(),
            )
            .await;
        }
        return Ok(Json(result));
    }
    let stored = match preloaded {
        Some(row) => Some(row),
        None => {
            state
                .store
                .get_interaction(interaction_id.as_id().as_str().to_string())
                .await?
        }
    };
    // Node owns first-answer-wins. Never commit an answer in the Hub before
    // the owner is reached, and let the Node reconcile same-command retries.
    let mut params = json!({
        "interactionId": interaction_id.as_id().as_str(),
        "commandId": command_id.as_id().as_str(),
        "byDevice": by_device.as_str(),
        "answer": body.answer,
    });
    if let Some(relay) = &bot_relay {
        // Informational only: the Node still authorizes against `byDevice`.
        // The durable attribution of the human owner lives in the Hub audit row.
        params["actingOpenId"] = json!(relay.open_id);
    }
    let hosts = match stored {
        Some(row) => vec![row.host_id],
        None => state.nodes.host_ids().await,
    };
    for host_id in hosts {
        match state
            .nodes
            .call(
                &host_id,
                "interaction.answer",
                params.clone(),
                NODE_RPC_TIMEOUT,
            )
            .await
        {
            Ok(Some(frame)) => match rpc_result(frame) {
                Ok(result) => {
                    // Mirror the owner's successful CAS immediately; a delayed
                    // journal flush must not resurrect a just-answered card.
                    state
                        .store
                        .record_interaction_answer(interaction_id.as_id().to_string())
                        .await?;
                    if let Some(relay) = &bot_relay {
                        settle_bot_relay(
                            &state,
                            &device_id,
                            relay,
                            interaction_id.as_id().as_str(),
                            command_id.as_id().as_str(),
                        )
                        .await;
                    }
                    return Ok(Json(result));
                }
                Err(HubError::NotFound) => {}
                Err(err) => return Err(err),
            },
            Ok(None) => {}
            Err(err) => return Err(err),
        }
    }

    Err(HubError::NotFound)
}

/// Winning-relay side effects: the Hub owns the authoritative ticket row, so
/// flip it to `answered` here (rather than trusting the dispatcher client),
/// and write the audit row that names the acting owner. D-005/D-011: the bot
/// never self-attests — the human `open_id` behind every relayed approval is
/// auditable and names the acting owner, not the bot device alone.
async fn settle_bot_relay(
    state: &AppState,
    device_id: &str,
    relay: &BotRelay,
    interaction_id: &str,
    command_id: &str,
) {
    if let Err(err) = state
        .store
        .set_card_ticket_state(
            relay.ticket_id.clone(),
            device_id.to_string(),
            "answered".into(),
            None,
        )
        .await
    {
        tracing::error!(error = %err, interaction_id, "failed to settle card ticket state");
    }
    if let Err(err) = state
        .store
        .append_audit(
            device_id.to_string(),
            "interaction.bot-answer".into(),
            Some(interaction_id.to_string()),
            json!({
                "actingOpenId": relay.open_id,
                "ticketId": relay.ticket_id,
                "commandId": command_id,
                "relayedBy": "feishu",
            }),
        )
        .await
    {
        tracing::error!(error = %err, interaction_id, "failed to audit bot-relayed answer");
    }
}

fn rpc_result(frame: Value) -> Result<Value, HubError> {
    if let Some(err) = frame.get("error") {
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(-32603);
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("node error");
        return Err(match code {
            -32005 => HubError::Expired,
            -32004 => HubError::Superseded {
                winner: err
                    .pointer("/data/winner")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            },
            -32602 if message.contains("not found") => HubError::NotFound,
            -32602 => HubError::BadRequest(message.to_string()),
            _ if message.contains("expired") => HubError::Expired,
            _ if message.contains("already answered") => HubError::Superseded {
                winner: String::new(),
            },
            _ if message.contains("not found") => HubError::NotFound,
            _ => HubError::Internal(message.to_string()),
        });
    }
    Ok(frame.get("result").cloned().unwrap_or(Value::Null))
}

// Retain the Node list wrapper for existing clients while exposing the complete
// Interaction entity at the top level consumed by the web API.
fn flatten_interaction(mut item: Value) -> Value {
    if let Some(entity) = item.get("interaction").and_then(Value::as_object).cloned()
        && let Some(object) = item.as_object_mut()
    {
        object.extend(entity);
    }
    item
}

/// Test-only seam for the D-051 global switch. `Some(..)` replaces the
/// environment-derived global for the whole test process (per-project
/// `FORCE_ON`/`FORCE_OFF` lists still apply); `None` restores env/default-off.
/// Tests that depend on the value must serialize against each other — the
/// override is intentionally process-global, like the env var it stands in for.
#[doc(hidden)]
pub mod delegated_decisions_test_support {
    /// Override the global `REMUDA_DELEGATED_DECISIONS` switch in tests.
    pub fn set_global_override(value: Option<bool>) {
        *super::DELEGATED_GLOBAL_OVERRIDE.write().unwrap() = value;
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_delegated_decisions;

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn switch_defaults_to_the_global_when_no_project_or_list_matches() {
        let on = ids(&[]);
        let off = ids(&[]);
        assert!(!resolve_delegated_decisions(false, &on, &off, None));
        assert!(resolve_delegated_decisions(true, &on, &off, None));
        assert!(!resolve_delegated_decisions(
            false,
            &on,
            &off,
            Some("prj_x")
        ));
        assert!(resolve_delegated_decisions(true, &on, &off, Some("prj_x")));
    }

    #[test]
    fn force_on_admits_one_project_while_the_global_stays_off() {
        let on = ids(&["prj_rolled_out"]);
        let off = ids(&[]);
        assert!(resolve_delegated_decisions(
            false,
            &on,
            &off,
            Some("prj_rolled_out")
        ));
        // Other projects keep the global setting.
        assert!(!resolve_delegated_decisions(
            false,
            &on,
            &off,
            Some("prj_other")
        ));
    }

    #[test]
    fn force_off_refuses_one_project_even_when_the_global_is_on() {
        let on = ids(&[]);
        let off = ids(&["prj_holdout"]);
        assert!(!resolve_delegated_decisions(
            true,
            &on,
            &off,
            Some("prj_holdout")
        ));
        assert!(resolve_delegated_decisions(
            true,
            &on,
            &off,
            Some("prj_other")
        ));
    }

    #[test]
    fn appearing_on_both_lists_fails_closed() {
        let on = ids(&["prj_ambiguous"]);
        let off = ids(&["prj_ambiguous"]);
        assert!(!resolve_delegated_decisions(
            true,
            &on,
            &off,
            Some("prj_ambiguous")
        ));
    }
}
