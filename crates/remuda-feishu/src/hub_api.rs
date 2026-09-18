//! [`InstanceApi`] over [`remuda_hub_client::HubClient`].

use std::future::Future;
use std::time::{SystemTime, UNIX_EPOCH};

use remuda_hub_client::{HubClient, InstanceCreate, pick_host};
use serde_json::{Value, json};

use crate::dispatcher::{
    CreateRequest, CreatedInstance, FollowEvent, FollowPage, InstanceApi, RespondRequest,
    SendRequest,
};
use crate::error::Error;
use crate::tickets::{StoredTicket, TicketBackend, state_wire};
use remuda_protocol::{AgentKind, InstanceId, Interaction};

/// Hub-backed [`InstanceApi`] (create/send/cancel/respond/follow).
pub struct HubInstanceApi {
    client: HubClient,
}

impl HubInstanceApi {
    /// Wrap an authenticated (or bootstrap) Hub client.
    #[must_use]
    pub fn new(client: HubClient) -> Self {
        Self { client }
    }

    /// Inner HTTP client.
    #[must_use]
    pub fn client(&self) -> &HubClient {
        &self.client
    }
}

impl InstanceApi for HubInstanceApi {
    fn create(
        &self,
        req: CreateRequest,
    ) -> impl Future<Output = Result<CreatedInstance, Error>> + Send {
        let client = &self.client;
        async move {
            let mut body = InstanceCreate {
                kind: Some(agent_wire(req.agent).to_string()),
                driver: Some(driver_for(req.agent).to_string()),
                prompt: Some(req.prompt.clone()),
                title: Some(req.session_key.as_str().to_string()),
                model: req.model.clone(),
                ..InstanceCreate::default()
            };
            if req.host.starts_with("hst_") {
                body.host_id = Some(req.host.clone());
                body.placement = Some(json!({ "host": req.host }));
            } else if !req.host.is_empty() {
                let labels = vec![req.host.clone()];
                body.placement = Some(json!({ "labels": labels }));
                if let Ok(hosts) = client.list_hosts().await
                    && let Ok(host_id) = pick_host(&hosts, &labels)
                {
                    body.host_id = Some(host_id);
                }
            } else {
                body.placement = Some(json!({ "kind": "any" }));
                if let Ok(hosts) = client.list_hosts().await
                    && let Ok(host_id) = pick_host(&hosts, &[])
                {
                    body.host_id = Some(host_id);
                }
            }
            let created = client.create_instance_typed(&body).await.map_err(hub_err)?;
            let instance_id = InstanceId::try_from(created.instance.instance_id)
                .map_err(|err| Error::InstanceApi(err.to_string()))?;
            Ok(CreatedInstance { instance_id })
        }
    }

    fn send(&self, req: SendRequest) -> impl Future<Output = Result<(), Error>> + Send {
        let client = &self.client;
        async move {
            let id = req.instance_id.as_id().as_str();
            let payload = json!({
                "instanceId": id,
                "input": {
                    "type": "prompt",
                    "mode": "new-turn",
                    "blocks": [{ "type": "text", "text": req.prompt }],
                    "origin": "bot",
                },
                "completionScope": "native-turn",
            });
            client
                .post_command(id, "instance.send", payload, Some(&req.idempotency_key))
                .await
                .map_err(hub_err)?;
            Ok(())
        }
    }

    fn cancel(&self, instance_id: &InstanceId) -> impl Future<Output = Result<(), Error>> + Send {
        let client = &self.client;
        let id = instance_id.as_id().as_str().to_string();
        async move {
            let payload = json!({ "instanceId": id });
            client
                .post_command(&id, "instance.cancel", payload, None)
                .await
                .map_err(hub_err)?;
            Ok(())
        }
    }

    fn respond(&self, req: RespondRequest) -> impl Future<Output = Result<(), Error>> + Send {
        let client = &self.client;
        async move {
            let payload = json!({
                "interactionId": req.interaction_id.as_id().as_str(),
                "requestVersion": req.request_version,
                "processGeneration": req.process_generation,
                "answer": req.answer,
                // Owner who clicked; the Hub 403s unless this open_id is on
                // the bot device's allowlist (design §5.1 #1).
                "actingOpenId": req.acting_open_id,
            });
            client
                .post(
                    &format!("/v1/interactions/{}/answer", req.interaction_id.as_id()),
                    &payload,
                )
                .await
                .map_err(hub_err)?;
            Ok(())
        }
    }

    fn follow(
        &self,
        instance_id: &InstanceId,
        after_seq: u64,
    ) -> impl Future<Output = Result<FollowPage, Error>> + Send {
        let client = &self.client;
        let id = instance_id.as_id().as_str().to_string();
        async move {
            let page = client
                // before_seq: None — this poller still advances its cursor
                // from durable_seq (below) and does not descend bounded
                // windows; see the c-journalpage handback.
                .get_journal_typed(&id, Some(&after_seq.to_string()), None)
                .await
                .map_err(hub_err)?;
            let next_seq = page.durable_seq.parse::<u64>().unwrap_or(after_seq);
            let events = page.events.iter().filter_map(map_journal_event).collect();
            Ok(FollowPage { events, next_seq })
        }
    }
}

fn hub_err(err: remuda_hub_client::ClientError) -> Error {
    Error::InstanceApi(err.to_string())
}

fn agent_wire(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Claude => "claude",
        AgentKind::Codex => "codex",
        AgentKind::Grok => "grok",
        AgentKind::Agy => "agy",
        AgentKind::Generic => "generic",
        AgentKind::Terminal => "terminal",
    }
}

fn driver_for(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Claude => "claude-print",
        AgentKind::Codex => "codex-appserver",
        AgentKind::Grok => "grok-acp",
        AgentKind::Agy => "agy-print",
        AgentKind::Generic => "generic-pty",
        AgentKind::Terminal => "shell-pty",
    }
}

/// Map one durable journal record onto the dispatcher's normalized vocabulary.
///
/// Driven by the real `ObservationKind` wire values
/// (`crates/remuda-protocol/src/enums.rs`): `tool_call` / `tool_result` drive
/// progress cards, `lifecycle` (run/instance terminal states) drives the
/// completion card, `interaction.requested` drives interaction cards, and
/// assistant `message` records are ignored for IM (no per-token cards).
fn map_journal_event(record: &Value) -> Option<FollowEvent> {
    let event = record.get("event").unwrap_or(record);
    let kind = event
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let payload = event.get("payload").cloned().unwrap_or(Value::Null);
    match kind {
        // Tool activity: either edge of a tool call refreshes the progress card.
        "tool_call" | "tool_result" => Some(FollowEvent::ToolBoundary {
            name: tool_name(kind, &payload),
            summary: tool_summary(kind, &payload),
            elapsed_secs: payload
                .get("elapsedSecs")
                .or_else(|| payload.get("elapsed_secs"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
        }),
        "interaction.requested" | "interactionRequested" | "interaction" => {
            let value = payload.get("interaction").cloned().unwrap_or(payload);
            serde_json::from_value::<Interaction>(value)
                .ok()
                .map(|interaction| FollowEvent::Interaction(Box::new(interaction)))
        }
        // Terminal entity lifecycle events complete the run card. Native
        // lifecycles are transient harness state and never complete a run.
        "lifecycle" if is_terminal_lifecycle(&payload) => Some(FollowEvent::Completed {
            conclusion: lifecycle_conclusion(&payload),
            ok: lifecycle_ok(&payload),
        }),
        // Assistant message records carry the run's text; the completion card
        // waits for the lifecycle edge instead. User/tool messages never post.
        _ => None,
    }
}

/// Extract a displayable tool name from a `tool_call`/`tool_result` payload.
///
/// `toolName` is a protocol `Knowledge` value, so it may be a bare string or
/// `{"state":"known","value":"Read"}`; tolerate older flat shapes too.
fn tool_name(kind: &str, payload: &Value) -> String {
    if kind == "tool_call" {
        knowledge_str(payload.get("toolName"))
            .or_else(|| {
                payload
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .or_else(|| {
                payload
                    .get("tool")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "tool".to_string())
    } else {
        // tool_result has no name; callers pair it with the tool_call they saw.
        "tool".to_string()
    }
}

/// One-line summary for the progress card body.
fn tool_summary(kind: &str, payload: &Value) -> String {
    if kind == "tool_call" {
        knowledge_str(payload.get("displayTitle"))
            .or_else(|| {
                payload
                    .get("summary")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_default()
    } else {
        // Prefer the first text block of the result, then structured fields.
        if let Some(text) = payload.pointer("/blocks/0/text").and_then(Value::as_str) {
            return text.to_string();
        }
        payload
            .get("summary")
            .or_else(|| payload.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }
}

/// Read a `Knowledge<T>`-shaped value (`"x"` or `{"state":"known","value":"x"}`).
fn knowledge_str(value: Option<&Value>) -> Option<String> {
    let value = value?;
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Object(_) => value
            .get("value")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        _ => None,
    }
}

/// Entity lifecycle states that mean a run or instance has stopped.
const TERMINAL_STATES: &[&str] = &["succeeded", "failed", "exited", "cancelled", "canceled"];

/// Only `lifecycle`/`entity` payloads for a run or instance can complete the
/// card. Native lifecycle events (model switches, prompts) must not.
fn is_terminal_lifecycle(payload: &Value) -> bool {
    if payload.get("type").and_then(Value::as_str) != Some("entity") {
        return false;
    }
    let entity_type = payload.get("entityType").and_then(Value::as_str);
    matches!(entity_type, Some("run") | Some("instance"))
        && payload
            .get("state")
            .and_then(Value::as_str)
            .is_some_and(|state| TERMINAL_STATES.contains(&state))
}

fn lifecycle_ok(payload: &Value) -> bool {
    !matches!(
        payload.get("state").and_then(Value::as_str),
        Some("failed" | "cancelled" | "canceled")
    )
}

fn lifecycle_conclusion(payload: &Value) -> String {
    let state = payload
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("done");
    if let Some(reason) = payload
        .get("reasonCode")
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty())
    {
        format!("{state}: {reason}")
    } else {
        state.to_string()
    }
}

/// Register the Feishu owner `open_id`s a Bot token may answer on behalf of
/// (design §5.1 #1). Called once on dispatcher boot. Failing to register means
/// every relayed answer is refused at the Hub — fail closed, never open.
pub async fn register_owner_allowlist(
    client: &HubClient,
    owner_open_ids: &[String],
) -> Result<(), Error> {
    client
        .post(
            "/v1/bot/owner-allowlist",
            &json!({ "ownerOpenIds": owner_open_ids }),
        )
        .await
        .map_err(hub_err)?;
    Ok(())
}

fn unix_ms(now: SystemTime) -> i64 {
    now.duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Durable [`TicketBackend`] backed by the Hub `card_tickets` table.
///
/// The Hub is the authoritative home of ticket ↔ instance bindings (design
/// §5.1 #3); this client is a thin write-through. A dispatcher restart
/// re-hydrates through [`TicketBackend::load_open`], and the same rows are the
/// binding the Hub checks when the bot relays an owner answer.
#[derive(Clone)]
pub struct HubTicketBackend {
    client: HubClient,
}

impl HubTicketBackend {
    /// Wrap an authenticated Hub client (the dispatcher's Bot token).
    #[must_use]
    pub fn new(client: HubClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl TicketBackend for HubTicketBackend {
    async fn put_ticket(&self, ticket: &StoredTicket) -> Result<(), Error> {
        self.client
            .post(
                "/v1/bot/card-tickets",
                &json!({
                    "ticketId": ticket.ticket_id,
                    "interactionId": ticket.interaction_id,
                    "instanceId": ticket.instance_id,
                    "sessionKey": ticket.session_key,
                    "cardMessageId": ticket.card_message_id,
                    "state": "open",
                    "requestVersion": ticket.request_version,
                    "processGeneration": ticket.process_generation,
                    "requestJson": ticket.request_json,
                    "createdAtMs": ticket.created_at_ms,
                    "expiresAtMs": ticket.expires_at_ms,
                }),
            )
            .await
            .map_err(hub_err)?;
        Ok(())
    }

    async fn set_ticket_state(
        &self,
        ticket_id: &str,
        state: crate::tickets::TicketState,
        card_message_id: Option<&str>,
    ) -> Result<(), Error> {
        self.client
            .post(
                &format!("/v1/bot/card-tickets/{ticket_id}/state"),
                &json!({
                    "state": state_wire(state),
                    "cardMessageId": card_message_id,
                }),
            )
            .await
            .map_err(hub_err)?;
        Ok(())
    }

    async fn expire_session(&self, session_key: &str) -> Result<Vec<String>, Error> {
        let result = self
            .client
            .post(
                "/v1/bot/card-tickets/expire-session",
                &json!({ "sessionKey": session_key }),
            )
            .await
            .map_err(hub_err)?;
        Ok(result
            .get("expiredTicketIds")
            .and_then(Value::as_array)
            .map(|ids| {
                ids.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn load_open(&self, now: SystemTime) -> Result<Vec<StoredTicket>, Error> {
        let result = self
            .client
            .get(&format!("/v1/bot/card-tickets?nowMs={}", unix_ms(now)))
            .await
            .map_err(hub_err)?;
        let Some(rows) = result.get("tickets").and_then(Value::as_array) else {
            return Ok(Vec::new());
        };
        let mut tickets = Vec::new();
        for row in rows {
            tickets.push(serde_json::from_value::<StoredTicket>(row.clone())?);
        }
        Ok(tickets)
    }
}
