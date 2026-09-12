//! Interaction ↔ card tickets. Runtime deadline 10–15 min (default 12), first writer wins.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use remuda_protocol::{
    ApprovalAnswer, DecisionEffect, Interaction, InteractionAnswer, InteractionId,
    InteractionRequest, QuestionField, QuestionFieldAnswer, QuestionInput, U64,
};
use serde_json::Value;

use crate::cards::{render_expired_card, render_interaction_card, render_recorded_card};
use crate::error::Error;
use crate::inbound::{CallbackValue, CardAction};

/// Default Interaction answer deadline (mid-range of 10–15 min; Feishu token is 30 min).
pub const DEFAULT_INTERACTION_TTL: Duration = Duration::from_secs(12 * 60);
/// Inclusive lower bound.
pub const MIN_INTERACTION_TTL: Duration = Duration::from_secs(10 * 60);
/// Inclusive upper bound.
pub const MAX_INTERACTION_TTL: Duration = Duration::from_secs(15 * 60);

/// Lifecycle of a card-backed Interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketState {
    /// Waiting for an owner click/submit.
    Open,
    /// First valid answer committed (native resolution may still be pending).
    Answered,
    /// Past `expires_at`.
    Expired,
}

/// Opaque card ticket stored by the dispatcher (callback `tid` is [`Self::ticket_id`]).
#[derive(Debug, Clone)]
pub struct CardTicket {
    /// Short id placed in `behaviors.callback.value.tid`.
    pub ticket_id: String,
    /// Protocol Interaction id.
    pub interaction_id: InteractionId,
    /// CAS field copied from the Interaction.
    pub request_version: U64,
    /// CAS field copied from the Interaction.
    pub process_generation: U64,
    /// Request used to encode/decode answers.
    pub request: InteractionRequest,
    /// Session this card was sent to.
    pub session_key: String,
    /// When the ticket was issued.
    pub created_at: SystemTime,
    /// Runtime deadline.
    pub expires_at: SystemTime,
    /// Open / answered / expired.
    pub state: TicketState,
}

/// First-writer-wins result of a card action.
#[derive(Debug, Clone)]
pub struct MappedAnswer {
    /// Ticket after the transition.
    pub ticket: CardTicket,
    /// Protocol answer for `interaction.respond`.
    pub answer: InteractionAnswer,
    /// Replacement card (`recorded` on success).
    pub replacement_card: Value,
}

/// In-memory ticket map. Hub persistence is out of scope for this prototype.
#[derive(Debug, Clone)]
pub struct TicketStore {
    ttl: Duration,
    tickets: BTreeMap<String, CardTicket>,
    by_interaction: BTreeMap<String, String>,
}

impl TicketStore {
    /// `ttl` must be in 10–15 minutes.
    pub fn new(ttl: Duration) -> Result<Self, Error> {
        if ttl < MIN_INTERACTION_TTL || ttl > MAX_INTERACTION_TTL {
            return Err(Error::InvalidTtl(ttl));
        }
        Ok(Self {
            ttl,
            tickets: BTreeMap::new(),
            by_interaction: BTreeMap::new(),
        })
    }

    /// 12 minute default.
    #[must_use]
    pub fn with_default_ttl() -> Self {
        Self {
            ttl: DEFAULT_INTERACTION_TTL,
            tickets: BTreeMap::new(),
            by_interaction: BTreeMap::new(),
        }
    }

    /// Configured TTL.
    #[must_use]
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Issue (or reuse) a ticket for a pending Interaction and render its card.
    ///
    /// Replays of the same `(interaction_id, request_version)` do not mint a second card.
    pub fn issue(
        &mut self,
        interaction: &Interaction,
        session_key: &str,
        now: SystemTime,
    ) -> Result<(CardTicket, Value), Error> {
        let iid = interaction.meta.id.as_id().as_str().to_string();
        if let Some(tid) = self.by_interaction.get(&iid)
            && let Some(existing) = self.tickets.get(tid)
            && existing.request_version == interaction.request_version
            && existing.state == TicketState::Open
            && existing.expires_at > now
        {
            let card = render_interaction_card(interaction, &existing.ticket_id)?;
            return Ok((existing.clone(), card));
        }
        if let Some(old_tid) = self.by_interaction.get(&iid).cloned()
            && let Some(old) = self.tickets.get_mut(&old_tid)
        {
            old.state = TicketState::Expired;
        }
        let ticket_id = new_ticket_id();
        let ticket = CardTicket {
            ticket_id: ticket_id.clone(),
            interaction_id: interaction.meta.id.clone(),
            request_version: interaction.request_version,
            process_generation: interaction.request_key.process_generation,
            request: interaction.request.clone(),
            session_key: session_key.to_string(),
            created_at: now,
            expires_at: now + self.ttl,
            state: TicketState::Open,
        };
        let card = render_interaction_card(interaction, &ticket_id)?;
        self.tickets.insert(ticket_id.clone(), ticket.clone());
        self.by_interaction.insert(iid, ticket_id);
        Ok((ticket, card))
    }

    /// Map a card callback onto an InteractionAnswer. First valid writer wins.
    pub fn answer_card(
        &mut self,
        action: &CardAction,
        now: SystemTime,
    ) -> Result<MappedAnswer, Error> {
        self.answer_card_parsed(action, now)
    }

    /// Map `{tid,a}` plus optional form payload.
    pub fn answer_callback(
        &mut self,
        callback: &CallbackValue,
        form: Option<&Value>,
        now: SystemTime,
    ) -> Result<MappedAnswer, Error> {
        let ticket = self
            .tickets
            .get(&callback.tid)
            .cloned()
            .ok_or_else(|| Error::TicketNotFound(callback.tid.clone()))?;
        if ticket.state == TicketState::Answered {
            return Err(Error::TicketAnswered(callback.tid.clone()));
        }
        if ticket.state == TicketState::Expired || now >= ticket.expires_at {
            if let Some(stored) = self.tickets.get_mut(&callback.tid) {
                stored.state = TicketState::Expired;
            }
            return Err(Error::TicketExpired(callback.tid.clone()));
        }
        let answer = encode_answer(&ticket.request, callback, form)?;
        if let Some(stored) = self.tickets.get_mut(&callback.tid) {
            stored.state = TicketState::Answered;
        }
        let mut updated = ticket;
        updated.state = TicketState::Answered;
        Ok(MappedAnswer {
            ticket: updated,
            answer,
            replacement_card: render_recorded_card("Answer recorded")?,
        })
    }

    /// Mark due tickets expired and return expired-card JSON for each.
    pub fn expire_due(&mut self, now: SystemTime) -> Result<Vec<(CardTicket, Value)>, Error> {
        let mut out = Vec::new();
        let ids: Vec<String> = self
            .tickets
            .iter()
            .filter(|(_, t)| t.state == TicketState::Open && now >= t.expires_at)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some(ticket) = self.tickets.get_mut(&id) {
                ticket.state = TicketState::Expired;
                let card = render_expired_card("Expired")?;
                out.push((ticket.clone(), card));
            }
        }
        Ok(out)
    }

    /// Lookup by callback tid.
    #[must_use]
    pub fn get(&self, ticket_id: &str) -> Option<&CardTicket> {
        self.tickets.get(ticket_id)
    }

    /// Newest still-open ticket for `session_key`, if the deadline has not passed.
    #[must_use]
    pub fn latest_open(&self, session_key: &str, now: SystemTime) -> Option<&CardTicket> {
        self.tickets
            .values()
            .filter(|ticket| {
                ticket.session_key == session_key
                    && ticket.state == TicketState::Open
                    && now < ticket.expires_at
            })
            .max_by_key(|ticket| ticket.created_at)
    }

    /// Decode a card action, parsing string `form_value` when needed.
    pub fn answer_card_parsed(
        &mut self,
        action: &CardAction,
        now: SystemTime,
    ) -> Result<MappedAnswer, Error> {
        let callback = action.callback()?;
        let parsed = match action.form_payload() {
            None => None,
            Some(v) => Some(parse_form_value(Some(v))?),
        };
        self.answer_callback(&callback, parsed.as_ref(), now)
    }
}

fn new_ticket_id() -> String {
    let raw = uuid::Uuid::new_v4().simple().to_string();
    raw.chars().take(10).collect()
}

fn encode_answer(
    request: &InteractionRequest,
    callback: &CallbackValue,
    form: Option<&Value>,
) -> Result<InteractionAnswer, Error> {
    match request {
        InteractionRequest::Approval(req) => {
            if callback.a == "submit" {
                return Err(Error::InvalidAnswer(
                    "approval cards do not accept form submit".into(),
                ));
            }
            let option = req
                .options
                .iter()
                .find(|o| o.id == callback.a)
                .or_else(|| {
                    effect_for_a(&callback.a)
                        .and_then(|effect| req.options.iter().find(|o| o.effect == effect))
                })
                .ok_or_else(|| Error::InvalidAnswer(format!("unknown action {}", callback.a)))?;
            Ok(InteractionAnswer::Approval(Box::new(ApprovalAnswer {
                option_id: option.id.clone(),
                input_digest: req.input_digest.clone(),
            })))
        }
        InteractionRequest::Question(req) => {
            if callback.a != "submit" {
                return Err(Error::InvalidAnswer(
                    "question cards only accept form submit".into(),
                ));
            }
            let parsed = parse_form_value(form)?;
            let object = parsed
                .as_object()
                .ok_or_else(|| Error::InvalidAnswer("form_value must be an object".into()))?;
            let mut answers = BTreeMap::new();
            for field in &req.fields {
                match field_answer(field, object.get(&field.id)) {
                    Ok(ans) => {
                        answers.insert(field.id.clone(), ans);
                    }
                    Err(err) if field.required => return Err(err),
                    Err(_) => {}
                }
            }
            Ok(InteractionAnswer::Question(Box::new(
                remuda_protocol::QuestionAnswer { answers },
            )))
        }
        InteractionRequest::PlanReview(_) | InteractionRequest::Elicitation(_) => {
            Err(Error::UnsupportedInteraction)
        }
    }
}

fn effect_for_a(a: &str) -> Option<DecisionEffect> {
    match a {
        "allow" => Some(DecisionEffect::AllowSession),
        "deny" => Some(DecisionEffect::Deny),
        "once" => Some(DecisionEffect::AllowOnce),
        _ => None,
    }
}

/// Parse `form_value` whether the bus gave an object or a JSON string.
pub fn parse_form_value(value: Option<&Value>) -> Result<Value, Error> {
    match value {
        None => Err(Error::InvalidAnswer("missing form_value".into())),
        Some(Value::Object(map)) => Ok(Value::Object(map.clone())),
        Some(Value::String(s)) => Ok(serde_json::from_str(s)?),
        Some(_) => Err(Error::InvalidAnswer(
            "form_value must be object or JSON string".into(),
        )),
    }
}

fn field_answer(field: &QuestionField, raw: Option<&Value>) -> Result<QuestionFieldAnswer, Error> {
    let Some(raw) = raw else {
        return Err(Error::InvalidAnswer(format!("missing field {}", field.id)));
    };
    match field.input {
        QuestionInput::Text => {
            let text = match raw {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            Ok(QuestionFieldAnswer {
                option_ids: Vec::new(),
                text: Some(text),
            })
        }
        QuestionInput::SingleSelect => {
            let id = match raw {
                Value::String(s) => s.clone(),
                Value::Array(arr) => arr
                    .first()
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                other => other.as_str().unwrap_or("").to_string(),
            };
            if id.is_empty() {
                return Err(Error::InvalidAnswer(format!("empty select {}", field.id)));
            }
            if !field.options.iter().any(|o| o.id == id) {
                return Err(Error::InvalidAnswer(format!(
                    "unknown option {id} for {}",
                    field.id
                )));
            }
            Ok(QuestionFieldAnswer {
                option_ids: vec![id],
                text: None,
            })
        }
        QuestionInput::MultiSelect => {
            let ids: Vec<String> = match raw {
                Value::String(s) => s
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect(),
                Value::Array(arr) => arr
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
                _ => Vec::new(),
            };
            for id in &ids {
                if !field.options.iter().any(|o| o.id == *id) {
                    return Err(Error::InvalidAnswer(format!(
                        "unknown option {id} for {}",
                        field.id
                    )));
                }
            }
            if field.required && ids.is_empty() {
                return Err(Error::InvalidAnswer(format!("missing field {}", field.id)));
            }
            Ok(QuestionFieldAnswer {
                option_ids: ids,
                text: None,
            })
        }
    }
}
