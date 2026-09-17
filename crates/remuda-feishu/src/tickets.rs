//! Interaction ↔ card tickets. Runtime deadline 10–15 min (default 12), first writer wins.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use remuda_protocol::{
    ApprovalAnswer, DecisionEffect, Interaction, InteractionAnswer, InteractionId,
    InteractionRequest, QuestionField, QuestionFieldAnswer, QuestionInput, U64,
};
use serde::{Deserialize, Serialize};
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
    /// Message id of the posted card, once the outbound receipt is known.
    ///
    /// A `/yes` that replies to this message binds to this ticket without a typed id.
    pub card_message_id: Option<String>,
    /// When the ticket was issued.
    pub created_at: SystemTime,
    /// Runtime deadline.
    pub expires_at: SystemTime,
    /// Open / answered / expired.
    pub state: TicketState,
}

/// Who is allowed to answer a ticket (F9: the answer must reach its own session).
///
/// Messages carry full topic identity, so they bind to the exact `session_key`.
/// `card.action.trigger` has no `thread_id`/`root_id` (`inbound.rs` builds an empty
/// [`crate::ThreadRef`]), so a card in a topic would never match the topic session key —
/// cards bind to the chat instead, which is still the gate the message path enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerScope<'a> {
    /// Ticket must belong to exactly this `session_key`.
    Session(&'a str),
    /// Ticket's session must live in this `chat_id`.
    Chat(&'a str),
}

impl AnswerScope<'_> {
    fn admits(&self, ticket: &CardTicket) -> bool {
        match self {
            Self::Session(key) => ticket.session_key == *key,
            Self::Chat(chat_id) => session_chat(&ticket.session_key) == Some(*chat_id),
        }
    }

    fn label(&self) -> &str {
        match self {
            Self::Session(key) => key,
            Self::Chat(chat_id) => chat_id,
        }
    }
}

/// `chat_id` inside a `feishu:{chat_id}:{thread||root||main}` session key.
#[must_use]
pub fn session_chat(session_key: &str) -> Option<&str> {
    let rest = session_key.strip_prefix("feishu:")?;
    let (chat_id, _suffix) = rest.rsplit_once(':')?;
    (!chat_id.is_empty()).then_some(chat_id)
}

/// Human-readable title of a pending request, for echoing what `/yes` approved.
#[must_use]
pub fn request_title(request: &InteractionRequest) -> &str {
    match request {
        InteractionRequest::Approval(req) => &req.title,
        InteractionRequest::Question(req) => &req.title,
        InteractionRequest::PlanReview(_) | InteractionRequest::Elicitation(_) => "request",
    }
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

/// In-memory ticket map with a write-through durable [`TicketBackend`].
#[derive(Clone)]
pub struct TicketStore {
    ttl: Duration,
    tickets: BTreeMap<String, CardTicket>,
    by_interaction: BTreeMap<String, String>,
    backend: Arc<dyn TicketBackend>,
}

impl std::fmt::Debug for TicketStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TicketStore")
            .field("ttl", &self.ttl)
            .field("tickets", &self.tickets)
            .field("by_interaction", &self.by_interaction)
            .finish_non_exhaustive()
    }
}

impl TicketStore {
    /// `ttl` must be in 10–15 minutes.
    pub fn new(ttl: Duration) -> Result<Self, Error> {
        Self::with_backend(ttl, Arc::new(NoopTicketBackend))
    }

    /// Like [`Self::new`] but with a write-through backend.
    pub fn with_backend(ttl: Duration, backend: Arc<dyn TicketBackend>) -> Result<Self, Error> {
        if ttl < MIN_INTERACTION_TTL || ttl > MAX_INTERACTION_TTL {
            return Err(Error::InvalidTtl(ttl));
        }
        Ok(Self {
            ttl,
            tickets: BTreeMap::new(),
            by_interaction: BTreeMap::new(),
            backend,
        })
    }

    /// 12 minute default.
    #[must_use]
    pub fn with_default_ttl() -> Self {
        Self {
            ttl: DEFAULT_INTERACTION_TTL,
            tickets: BTreeMap::new(),
            by_interaction: BTreeMap::new(),
            backend: Arc::new(NoopTicketBackend),
        }
    }

    /// 12 minute default with a write-through backend.
    #[must_use]
    pub fn with_default_ttl_backend(backend: Arc<dyn TicketBackend>) -> Self {
        Self {
            ttl: DEFAULT_INTERACTION_TTL,
            tickets: BTreeMap::new(),
            by_interaction: BTreeMap::new(),
            backend,
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
            card_message_id: None,
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
    ///
    /// `scope` must admit the ticket — a card action may only answer a ticket whose
    /// session lives in the same chat (F9).
    pub fn answer_card(
        &mut self,
        action: &CardAction,
        scope: AnswerScope<'_>,
        now: SystemTime,
    ) -> Result<MappedAnswer, Error> {
        self.answer_card_parsed(action, scope, now)
    }

    /// Map `{tid,a}` plus optional form payload, rejecting cross-session answers.
    pub fn answer_callback(
        &mut self,
        callback: &CallbackValue,
        form: Option<&Value>,
        scope: AnswerScope<'_>,
        now: SystemTime,
    ) -> Result<MappedAnswer, Error> {
        let ticket = self
            .tickets
            .get(&callback.tid)
            .cloned()
            .ok_or_else(|| Error::TicketNotFound(callback.tid.clone()))?;
        // F9: the ticket, the answering session, and the chat must agree. Without this
        // any owner could answer any other topic's pending call.
        if !scope.admits(&ticket) {
            return Err(Error::TicketScope {
                ticket_id: callback.tid.clone(),
                scope: scope.label().to_string(),
            });
        }
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

    /// Record the message id of the posted card so a threaded `/yes` can bind to it.
    pub fn set_card_message_id(&mut self, ticket_id: &str, message_id: &str) {
        if let Some(ticket) = self.tickets.get_mut(ticket_id) {
            ticket.card_message_id = Some(message_id.to_string());
        }
    }

    /// Newest still-open ticket for `session_key`, if the deadline has not passed.
    #[must_use]
    pub fn latest_open(&self, session_key: &str, now: SystemTime) -> Option<&CardTicket> {
        self.open_for_session(session_key, now)
            .max_by_key(|ticket| ticket.created_at)
    }

    /// Every still-open ticket for `session_key`, oldest ticket id first.
    pub fn open_for_session(
        &self,
        session_key: &str,
        now: SystemTime,
    ) -> impl Iterator<Item = &CardTicket> {
        self.tickets.values().filter(move |ticket| {
            ticket.session_key == session_key
                && ticket.state == TicketState::Open
                && now < ticket.expires_at
        })
    }

    /// Resolve the ticket a `/yes` `/no` shortcut refers to (F8).
    ///
    /// The shortcut never guesses: it binds to an explicit ticket id, to the card the
    /// message replies to, or — only when the session has exactly one pending card —
    /// to that unambiguous ticket. Two open tickets without a reference is an error,
    /// so the owner is told to name one rather than silently approving the newest.
    pub fn resolve_shortcut(
        &self,
        session_key: &str,
        reference: ShortcutRef<'_>,
        now: SystemTime,
    ) -> Result<&CardTicket, ShortcutMiss> {
        if let Some(ticket_id) = reference.ticket_id {
            return self
                .open_for_session(session_key, now)
                .find(|ticket| ticket.ticket_id == ticket_id)
                .ok_or_else(|| ShortcutMiss::UnknownTicket(ticket_id.to_string()));
        }
        if let Some(reply_to) = reference.reply_to
            && let Some(ticket) = self
                .open_for_session(session_key, now)
                .find(|ticket| ticket.card_message_id.as_deref() == Some(reply_to))
        {
            return Ok(ticket);
        }
        let mut open = self.open_for_session(session_key, now);
        let Some(only) = open.next() else {
            return Err(ShortcutMiss::NoneOpen);
        };
        if open.next().is_some() {
            let mut ids: Vec<String> = self
                .open_for_session(session_key, now)
                .map(|ticket| ticket.ticket_id.clone())
                .collect();
            ids.sort();
            return Err(ShortcutMiss::Ambiguous(ids));
        }
        Ok(only)
    }

    /// Expire every open ticket for a session (`/new`, `/stop`) so a later `/yes`
    /// cannot answer a retired instance's pending call.
    pub fn expire_session(&mut self, session_key: &str) -> Vec<String> {
        let mut expired = Vec::new();
        for ticket in self.tickets.values_mut() {
            if ticket.session_key == session_key && ticket.state == TicketState::Open {
                ticket.state = TicketState::Expired;
                expired.push(ticket.ticket_id.clone());
            }
        }
        expired
    }

    /// Decode a card action, parsing string `form_value` when needed.
    pub fn answer_card_parsed(
        &mut self,
        action: &CardAction,
        scope: AnswerScope<'_>,
        now: SystemTime,
    ) -> Result<MappedAnswer, Error> {
        let callback = action.callback()?;
        let parsed = match action.form_payload() {
            None => None,
            Some(v) => Some(parse_form_value(Some(v))?),
        };
        self.answer_callback(&callback, parsed.as_ref(), scope, now)
    }

    // ----- Durable wrappers (write-through to the TicketBackend) -----------

    /// Load every still-open ticket after a dispatcher restart. Local rows
    /// never overwrite a fresher binding for the same interaction.
    pub async fn hydrate(&mut self, now: SystemTime) -> Result<usize, Error> {
        let stored = self.backend.load_open(now).await?;
        let mut loaded = 0;
        for row in stored {
            if self.by_interaction.contains_key(&row.interaction_id) {
                continue;
            }
            let ticket = row.into_ticket(TicketState::Open)?;
            if ticket.expires_at <= now {
                continue;
            }
            self.by_interaction.insert(
                ticket.interaction_id.as_id().as_str().to_string(),
                ticket.ticket_id.clone(),
            );
            self.tickets.insert(ticket.ticket_id.clone(), ticket);
            loaded += 1;
        }
        Ok(loaded)
    }

    /// [`Self::issue`] plus write-through persistence. `instance_id` is the
    /// session's live instance, recorded in the durable binding.
    ///
    /// When the interaction is re-issued with a new request version, the local
    /// map expires the previous ticket; the durable row must be expired too or
    /// the Hub's one-open-ticket-per-interaction index rejects the new row.
    pub async fn issue_synced(
        &mut self,
        interaction: &Interaction,
        session_key: &str,
        instance_id: &str,
        now: SystemTime,
    ) -> Result<(CardTicket, Value), Error> {
        let iid = interaction.meta.id.as_id().as_str().to_string();
        let previous = self.by_interaction.get(&iid).cloned();
        let (ticket, card) = self.issue(interaction, session_key, now)?;
        if let Some(old_tid) = previous.filter(|tid| tid != &ticket.ticket_id) {
            self.backend
                .set_ticket_state(&old_tid, TicketState::Expired, None)
                .await?;
        }
        self.backend
            .put_ticket(&ticket.to_stored(instance_id)?)
            .await?;
        Ok((ticket, card))
    }

    /// [`Self::answer_card`] against the local first-writer-wins map.
    ///
    /// The durable row is deliberately left `open` until the caller confirms
    /// the Hub accepted the relay ([`Self::mark_answered_synced`]): flipping
    /// it earlier would make the Hub's own open-binding check refuse the very
    /// answer in flight.
    pub async fn answer_card_synced(
        &mut self,
        action: &CardAction,
        scope: AnswerScope<'_>,
        now: SystemTime,
    ) -> Result<MappedAnswer, Error> {
        self.answer_card(action, scope, now)
    }

    /// [`Self::answer_callback`] against the local map; durable state flips on
    /// [`Self::mark_answered_synced`], same reasoning as
    /// [`Self::answer_card_synced`].
    pub async fn answer_callback_synced(
        &mut self,
        callback: &CallbackValue,
        form: Option<&Value>,
        scope: AnswerScope<'_>,
        now: SystemTime,
    ) -> Result<MappedAnswer, Error> {
        self.answer_callback(callback, form, scope, now)
    }

    /// Flip the durable row to answered after the Hub accepted the relay.
    pub async fn mark_answered_synced(&self, ticket_id: &str) -> Result<(), Error> {
        self.backend
            .set_ticket_state(ticket_id, TicketState::Answered, None)
            .await
    }

    /// [`Self::expire_due`] plus write-through of each expired state.
    pub async fn expire_due_synced(
        &mut self,
        now: SystemTime,
    ) -> Result<Vec<(CardTicket, Value)>, Error> {
        let expired = self.expire_due(now)?;
        for (ticket, _) in &expired {
            self.backend
                .set_ticket_state(&ticket.ticket_id, TicketState::Expired, None)
                .await?;
        }
        Ok(expired)
    }

    /// [`Self::expire_session`] backed by the durable store, so a second
    /// dispatcher process cannot answer a retired instance's pending card.
    pub async fn expire_session_synced(&mut self, session_key: &str) -> Result<Vec<String>, Error> {
        let local = self.expire_session(session_key);
        let durable = self.backend.expire_session(session_key).await?;
        for id in &durable {
            if let Some(ticket) = self.tickets.get_mut(id)
                && ticket.state == TicketState::Open
            {
                ticket.state = TicketState::Expired;
            }
        }
        Ok(if local.is_empty() { durable } else { local })
    }

    /// [`Self::set_card_message_id`] plus write-through.
    pub async fn set_card_message_id_synced(
        &mut self,
        ticket_id: &str,
        message_id: &str,
    ) -> Result<(), Error> {
        self.set_card_message_id(ticket_id, message_id);
        self.backend
            .set_ticket_state(ticket_id, TicketState::Open, Some(message_id))
            .await
    }
}

/// Persisted snapshot of a ticket (design §5.1 #3): everything a restarted
/// dispatcher needs to re-hydrate the card binding without replaying journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredTicket {
    /// Short id placed in the card callback.
    pub ticket_id: String,
    /// Instance whose journal produced the Interaction.
    pub instance_id: String,
    /// Protocol Interaction id.
    pub interaction_id: String,
    /// CAS fields.
    pub request_version: String,
    /// CAS fields.
    pub process_generation: String,
    /// Tagged `InteractionRequest` JSON (so answers can be re-encoded).
    pub request_json: String,
    /// Session the card was sent to.
    pub session_key: String,
    /// Posted card message id, once known.
    #[serde(default)]
    pub card_message_id: Option<String>,
    /// Issue time, unix epoch milliseconds.
    pub created_at_ms: i64,
    /// Runtime deadline, unix epoch milliseconds.
    pub expires_at_ms: i64,
}

impl CardTicket {
    /// Serialize for the durable ticket store.
    pub fn to_stored(&self, instance_id: &str) -> Result<StoredTicket, Error> {
        Ok(StoredTicket {
            ticket_id: self.ticket_id.clone(),
            instance_id: instance_id.to_string(),
            interaction_id: self.interaction_id.as_id().as_str().to_string(),
            request_version: self.request_version.0.to_string(),
            process_generation: self.process_generation.0.to_string(),
            request_json: serde_json::to_string(&self.request)?,
            session_key: self.session_key.clone(),
            card_message_id: self.card_message_id.clone(),
            created_at_ms: unix_ms(self.created_at)?,
            expires_at_ms: unix_ms(self.expires_at)?,
        })
    }
}

impl StoredTicket {
    /// Reconstruct a ticket after a dispatcher restart.
    pub fn into_ticket(self, state: TicketState) -> Result<CardTicket, Error> {
        Ok(CardTicket {
            ticket_id: self.ticket_id,
            interaction_id: self.interaction_id.parse().map_err(
                |err: remuda_protocol::WireValueError| {
                    Error::SessionStore(format!("stored interaction id: {err}"))
                },
            )?,
            request_version: U64(self.request_version.parse().unwrap_or(0)),
            process_generation: U64(self.process_generation.parse().unwrap_or(0)),
            request: serde_json::from_str(&self.request_json)?,
            session_key: self.session_key,
            card_message_id: self.card_message_id,
            created_at: system_time_ms(self.created_at_ms),
            expires_at: system_time_ms(self.expires_at_ms),
            state,
        })
    }
}

fn unix_ms(now: SystemTime) -> Result<i64, Error> {
    now.duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .map_err(|_| Error::SessionStore("ticket time before unix epoch".into()))
}

fn system_time_ms(ms: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(u64::try_from(ms.max(0)).unwrap_or(0))
}

/// Durable home for ticket bindings. The in-process [`TicketStore`] is the
/// write-through cache; this backs it so a dispatcher restart keeps open cards.
#[async_trait::async_trait]
pub trait TicketBackend: Send + Sync {
    /// Upsert a ticket snapshot.
    async fn put_ticket(&self, ticket: &StoredTicket) -> Result<(), Error>;
    /// Update ticket state, optionally attaching the posted card message id.
    async fn set_ticket_state(
        &self,
        ticket_id: &str,
        state: TicketState,
        card_message_id: Option<&str>,
    ) -> Result<(), Error>;
    /// Expire every open ticket for a session; returns the affected ticket ids.
    async fn expire_session(&self, session_key: &str) -> Result<Vec<String>, Error>;
    /// All unexpired open tickets at `now`, for restart hydration.
    async fn load_open(&self, now: SystemTime) -> Result<Vec<StoredTicket>, Error>;
}

/// No persistence (unit tests exercising the synchronous mapping rules).
#[derive(Debug, Default, Clone)]
pub struct NoopTicketBackend;

#[async_trait::async_trait]
impl TicketBackend for NoopTicketBackend {
    async fn put_ticket(&self, _ticket: &StoredTicket) -> Result<(), Error> {
        Ok(())
    }
    async fn set_ticket_state(
        &self,
        _ticket_id: &str,
        _state: TicketState,
        _card_message_id: Option<&str>,
    ) -> Result<(), Error> {
        Ok(())
    }
    async fn expire_session(&self, _session_key: &str) -> Result<Vec<String>, Error> {
        Ok(Vec::new())
    }
    async fn load_open(&self, _now: SystemTime) -> Result<Vec<StoredTicket>, Error> {
        Ok(Vec::new())
    }
}

/// Wire name for a ticket state.
pub fn state_wire(state: TicketState) -> &'static str {
    match state {
        TicketState::Open => "open",
        TicketState::Answered => "answered",
        TicketState::Expired => "expired",
    }
}

/// How a `/yes` `/no` message names the ticket it answers (F8).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShortcutRef<'a> {
    /// Ticket id typed as an argument: `/yes t1a2b3c4d5`.
    pub ticket_id: Option<&'a str>,
    /// `message_id` the command replied to, matched against the posted card.
    pub reply_to: Option<&'a str>,
}

/// Why a `/yes` `/no` shortcut could not be bound to exactly one ticket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShortcutMiss {
    /// No open ticket in this session.
    NoneOpen,
    /// A ticket id was given but no open ticket in this session matches it.
    UnknownTicket(String),
    /// More than one card is pending and the command named none of them.
    Ambiguous(Vec<String>),
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
                // A non-empty free-text companion overrides the select, the
                // same way "Type something" does in the TUI.
                let free_text = object
                    .get(crate::cards::free_text_name(&field.id).as_str())
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty());
                if let Some(text) = free_text {
                    answers.insert(
                        field.id.clone(),
                        QuestionFieldAnswer {
                            option_ids: Vec::new(),
                            text: Some(text.to_owned()),
                        },
                    );
                    continue;
                }
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
        // `/yes` maps here (F8): a typed shortcut grants one use, never the session.
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
