//! Inbound normalize: consume NDJSON → [`Inbound`], with allowlist and commands.

use std::collections::{HashSet, VecDeque};

use remuda_protocol::AgentKind;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Error;
use crate::{EVENT_CARD_ACTION, EVENT_IM_RECEIVE};

/// `chat_type` from `im.message.receive_v1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatType {
    /// Direct message.
    P2p,
    /// Group chat (including topic-mode groups).
    Group,
}

/// Compact mention aligned with `im +messages-mget`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mention {
    /// Mentioned user `open_id` (`ou_…`).
    #[serde(default)]
    pub id: String,
    /// Placeholder such as `@_user_1`.
    #[serde(default)]
    pub key: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
}

/// Flattened `im.message.receive_v1` line (`jq_root_path` is `.`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImMessage {
    /// Chat id (`oc_…`).
    pub chat_id: String,
    /// `p2p` or `group`.
    pub chat_type: ChatType,
    /// Message id (`om_…`); inbound idempotency key.
    pub message_id: String,
    /// Delivery id; do not use for IM dedup.
    #[serde(default)]
    pub event_id: Option<String>,
    /// Sender `open_id`.
    pub sender_id: String,
    /// `user` or `bot`.
    #[serde(default)]
    pub sender_type: Option<String>,
    /// Mentions after convertlib flattening.
    #[serde(default)]
    pub mentions: Vec<Mention>,
    /// Topic id when present (`omt_…` or `om_…`).
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Reply-tree root when present.
    #[serde(default)]
    pub root_id: Option<String>,
    /// Direct parent when present.
    #[serde(default)]
    pub reply_to: Option<String>,
    /// `text` / `post` / `image` / `interactive` / …
    pub message_type: String,
    /// Pre-rendered text for most types; raw JSON string only for `interactive`.
    #[serde(default)]
    pub content: String,
    /// Always `im.message.receive_v1` when the bus sets it.
    #[serde(default, rename = "type")]
    pub event_type: Option<String>,
}

/// Flattened `card.action.trigger` line.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct CardAction {
    /// Card-update token (30 min / 2 uses). Never logged in full.
    #[serde(default)]
    pub token: String,
    /// Triggered element type (`button`, `form` submit, …).
    #[serde(default)]
    pub action_tag: Option<String>,
    /// Developer `behaviors.callback.value`, JSON object or JSON string.
    #[serde(default)]
    pub action_value: Option<Value>,
    /// Element `name`.
    #[serde(default)]
    pub action_name: Option<String>,
    /// Form values as object or JSON string.
    #[serde(default)]
    pub form_value: Option<Value>,
    /// Compatibility alias used by some SDK payloads.
    #[serde(default)]
    pub form_values: Option<Value>,
    /// Operator `open_id`.
    pub operator_id: String,
    /// Card message id.
    pub message_id: String,
    /// Chat id when the bus includes it.
    #[serde(default)]
    pub chat_id: Option<String>,
    /// Unique delivery id; preferred card-action idempotency key.
    #[serde(default)]
    pub event_id: Option<String>,
    /// Always `card.action.trigger` when the bus sets it.
    #[serde(default, rename = "type")]
    pub event_type: Option<String>,
}

impl std::fmt::Debug for CardAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CardAction")
            .field(
                "token",
                &if self.token.is_empty() {
                    ""
                } else {
                    "[redacted]"
                },
            )
            .field("action_tag", &self.action_tag)
            .field("action_value", &self.action_value)
            .field("action_name", &self.action_name)
            .field("form_value", &self.form_value)
            .field("form_values", &self.form_values)
            .field("operator_id", &self.operator_id)
            .field("message_id", &self.message_id)
            .field("chat_id", &self.chat_id)
            .field("event_id", &self.event_id)
            .field("event_type", &self.event_type)
            .finish()
    }
}

impl CardAction {
    /// Parsed `{tid, a}` from `action_value`.
    pub fn callback(&self) -> Result<CallbackValue, Error> {
        parse_callback_value(self.action_value.as_ref())
    }

    /// Form payload, preferring `form_value` then `form_values`.
    #[must_use]
    pub fn form_payload(&self) -> Option<&Value> {
        self.form_value.as_ref().or(self.form_values.as_ref())
    }
}

/// Short callback payload stored in `behaviors.callback.value`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallbackValue {
    /// Opaque ticket id (not the full Interaction id).
    pub tid: String,
    /// `allow` / `deny` / `once` / `submit`, or a DecisionOption id.
    pub a: String,
}

/// Thread identity used for session keys and reply-in-thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadRef {
    /// Topic id.
    pub thread_id: Option<String>,
    /// Reply-tree root.
    pub root_id: Option<String>,
    /// Direct parent.
    pub reply_to: Option<String>,
}

impl ThreadRef {
    /// `thread_id || root_id || "main"` — looking only at `thread_id` misses topic groups.
    #[must_use]
    pub fn session_suffix(&self) -> &str {
        nonempty(self.thread_id.as_deref())
            .or_else(|| nonempty(self.root_id.as_deref()))
            .unwrap_or("main")
    }
}

/// `feishu:{chat_id}:{thread_id||root_id||main}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey(pub String);

impl SessionKey {
    /// Build the dispatcher session key for one chat/topic.
    #[must_use]
    pub fn from_parts(chat_id: &str, thread: &ThreadRef) -> Self {
        Self(format!("feishu:{chat_id}:{}", thread.session_suffix()))
    }

    /// Wire spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Parsed consume line.
#[derive(Debug, Clone)]
pub enum RawEvent {
    /// IM message.
    Message(ImMessage),
    /// Card button or form submit.
    CardAction(CardAction),
}

/// Owner / chat / mention policy from bot-dispatcher §4.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InboundPolicy {
    /// Only these `open_id`s may issue commands or click cards.
    pub owner_open_ids: Vec<String>,
    /// Allowed `chat_id`s. Empty: p2p owners only; groups always need an entry.
    pub chat_allowlist: Vec<String>,
    /// Bot `open_id` used to detect `@bot`.
    pub bot_open_id: Option<String>,
    /// Bot display name used to detect `@bot`.
    pub bot_name: Option<String>,
    /// If true, allowlisted groups need not `@` the bot.
    pub allow_unaddressed: bool,
}

/// Why an event was not turned into [`Inbound`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropReason {
    /// `message_id` / card delivery already processed.
    Duplicate,
    /// Sender or card operator is not in `owner_open_ids`.
    OwnerNotAllowed,
    /// Chat is not on the allowlist (groups always require a list entry).
    ChatNotAllowed,
    /// Group message did not mention the bot.
    GroupRequiresMention,
    /// Sender is another bot.
    BotSender,
}

/// Admit or ignore a parsed event.
#[derive(Debug, Clone)]
pub enum GateDecision {
    /// Passes policy and is new.
    Take(Box<Inbound>),
    /// Silently ignored (and audited by the caller).
    Drop {
        /// Stable reason code.
        reason: DropReason,
    },
}

/// Normalized inbound event ready for the Hub durable inbox.
#[derive(Debug, Clone)]
pub struct Inbound {
    /// Session routing key.
    pub session_key: SessionKey,
    /// Idempotency key (IM: `message_id`; card: `event_id` when present).
    pub idempotency_key: String,
    /// Human `open_id` (sender or card operator).
    pub actor_open_id: String,
    /// Chat id.
    pub chat_id: String,
    /// Chat type when known (cards may omit it).
    pub chat_type: Option<ChatType>,
    /// Topic / reply identity.
    pub thread: ThreadRef,
    /// Payload.
    pub kind: InboundKind,
}

/// Body of an admitted event.
#[derive(Debug, Clone)]
pub enum InboundKind {
    /// Chat message after mention stripping and command parse.
    Message {
        /// Slash command or free-text prompt.
        intent: Intent,
        /// Native `message_type`.
        message_type: String,
        /// Original content (pre-rendered text; do not `fromjson` unless interactive).
        content: String,
        /// Mentions as received.
        mentions: Vec<Mention>,
        /// Source message id (reply target).
        message_id: String,
    },
    /// Card callback.
    CardAction {
        /// Parsed `{tid,a}` when present.
        callback: Option<CallbackValue>,
        /// Raw action for ticket mapping.
        action: Box<CardAction>,
    },
}

/// Explicit `/` commands; commands always win over routing heuristics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplicitCommand {
    /// Open a new agent session on this topic.
    New,
    /// Pin a remote host.
    Host {
        /// Host label, e.g. `devbox`.
        name: String,
    },
    /// Pin a CLI.
    Agent {
        /// `claude` / `codex` / `grok` / `agy`, or [`AgentKind::Generic`].
        kind: AgentKind,
        /// Original token.
        raw: String,
    },
    /// Pin a gateway model id.
    Model {
        /// Model id as typed.
        model: String,
    },
    /// Ask the runtime for session status.
    Status,
    /// Stop the current run/instance.
    Stop,
    /// Approval shortcut when the card is unavailable.
    Yes,
    /// Denial shortcut when the card is unavailable.
    No,
}

/// User intent extracted from a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// Whitelist slash command.
    Command(ExplicitCommand),
    /// Unknown `/…` token.
    UnknownCommand {
        /// Original line after mention strip.
        raw: String,
    },
    /// Ordinary prompt (possibly empty after stripping mentions).
    Prompt {
        /// Remaining text.
        text: String,
    },
}

/// Recent-key ring for consume replay.
#[derive(Debug, Clone)]
pub struct Deduper {
    order: VecDeque<String>,
    set: HashSet<String>,
    cap: usize,
}

impl Deduper {
    /// Remember up to `cap` keys (oldest evicted).
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            order: VecDeque::new(),
            set: HashSet::new(),
            cap: cap.max(1),
        }
    }

    /// Returns true if `key` was already seen.
    pub fn seen_or_insert(&mut self, key: String) -> bool {
        if self.set.contains(&key) {
            return true;
        }
        if self.order.len() >= self.cap
            && let Some(old) = self.order.pop_front()
        {
            self.set.remove(&old);
        }
        self.set.insert(key.clone());
        self.order.push_back(key);
        false
    }
}

impl Default for Deduper {
    fn default() -> Self {
        Self::new(4096)
    }
}

/// Parse one stdout line from `lark-cli event consume`.
pub fn parse_event_line(line: &str) -> Result<RawEvent, Error> {
    let line = line.trim();
    if line.is_empty() {
        return Err(Error::EmptyLine);
    }
    let value: Value = serde_json::from_str(line)?;
    let ty = value.get("type").and_then(Value::as_str).unwrap_or("");
    let has_token = value.get("token").is_some() && value.get("operator_id").is_some();
    let has_im = value.get("message_id").is_some() && value.get("chat_id").is_some();
    if ty == EVENT_CARD_ACTION || (ty.is_empty() && has_token) {
        let action: CardAction = serde_json::from_value(value)?;
        if action.operator_id.is_empty() || action.message_id.is_empty() {
            return Err(Error::MissingField("operator_id/message_id"));
        }
        return Ok(RawEvent::CardAction(action));
    }
    if ty == EVENT_IM_RECEIVE || (ty.is_empty() && has_im) {
        let mut msg: ImMessage = serde_json::from_value(value)?;
        if msg.chat_id.is_empty() || msg.message_id.is_empty() || msg.sender_id.is_empty() {
            return Err(Error::MissingField("chat_id/message_id/sender_id"));
        }
        msg.thread_id = empty_to_none(msg.thread_id.take());
        msg.root_id = empty_to_none(msg.root_id.take());
        msg.reply_to = empty_to_none(msg.reply_to.take());
        return Ok(RawEvent::Message(msg));
    }
    Err(Error::UnknownEventType(if ty.is_empty() {
        "<missing>".into()
    } else {
        ty.into()
    }))
}

/// Strip leading `@mentions` (convertlib may rewrite them to display names) and parse `/` commands.
pub fn parse_intent(content: &str) -> Intent {
    let stripped = strip_leading_mentions(content);
    let trimmed = stripped.trim();
    let Some(rest) = trimmed.strip_prefix('/') else {
        return Intent::Prompt {
            text: trimmed.to_string(),
        };
    };
    let mut parts = rest.split_whitespace();
    let Some(cmd) = parts.next() else {
        return Intent::Prompt {
            text: trimmed.to_string(),
        };
    };
    match cmd.to_ascii_lowercase().as_str() {
        "new" => Intent::Command(ExplicitCommand::New),
        "host" => {
            let name = parts.collect::<Vec<_>>().join(" ");
            if name.is_empty() {
                Intent::UnknownCommand {
                    raw: trimmed.to_string(),
                }
            } else {
                Intent::Command(ExplicitCommand::Host { name })
            }
        }
        "agent" => {
            let Some(raw) = parts.next() else {
                return Intent::UnknownCommand {
                    raw: trimmed.to_string(),
                };
            };
            Intent::Command(ExplicitCommand::Agent {
                kind: parse_agent(raw),
                raw: raw.to_string(),
            })
        }
        "model" => {
            let model = parts.collect::<Vec<_>>().join(" ");
            if model.is_empty() {
                Intent::UnknownCommand {
                    raw: trimmed.to_string(),
                }
            } else {
                Intent::Command(ExplicitCommand::Model { model })
            }
        }
        "status" => Intent::Command(ExplicitCommand::Status),
        "stop" => Intent::Command(ExplicitCommand::Stop),
        "yes" => Intent::Command(ExplicitCommand::Yes),
        "no" => Intent::Command(ExplicitCommand::No),
        _ => Intent::UnknownCommand {
            raw: trimmed.to_string(),
        },
    }
}

/// Apply allowlist, mention, and idempotency gates.
pub fn admit(
    event: RawEvent,
    policy: &InboundPolicy,
    dedup: &mut Deduper,
) -> Result<GateDecision, Error> {
    match event {
        RawEvent::Message(msg) => admit_message(msg, policy, dedup),
        RawEvent::CardAction(action) => admit_card(action, policy, dedup),
    }
}

fn admit_message(
    msg: ImMessage,
    policy: &InboundPolicy,
    dedup: &mut Deduper,
) -> Result<GateDecision, Error> {
    if dedup.seen_or_insert(msg.message_id.clone()) {
        return Ok(GateDecision::Drop {
            reason: DropReason::Duplicate,
        });
    }
    if msg.sender_type.as_deref() == Some("bot") {
        return Ok(GateDecision::Drop {
            reason: DropReason::BotSender,
        });
    }
    if !is_owner(&msg.sender_id, policy) {
        return Ok(GateDecision::Drop {
            reason: DropReason::OwnerNotAllowed,
        });
    }
    if !chat_allowed(&msg.chat_id, msg.chat_type, policy) {
        return Ok(GateDecision::Drop {
            reason: DropReason::ChatNotAllowed,
        });
    }
    if msg.chat_type == ChatType::Group && !policy.allow_unaddressed && !mentions_bot(&msg, policy)
    {
        return Ok(GateDecision::Drop {
            reason: DropReason::GroupRequiresMention,
        });
    }
    let thread = ThreadRef {
        thread_id: msg.thread_id.clone(),
        root_id: msg.root_id.clone(),
        reply_to: msg.reply_to.clone(),
    };
    let intent = parse_intent(&msg.content);
    Ok(GateDecision::Take(Box::new(Inbound {
        session_key: SessionKey::from_parts(&msg.chat_id, &thread),
        idempotency_key: msg.message_id.clone(),
        actor_open_id: msg.sender_id.clone(),
        chat_id: msg.chat_id.clone(),
        chat_type: Some(msg.chat_type),
        thread,
        kind: InboundKind::Message {
            intent,
            message_type: msg.message_type,
            content: msg.content,
            mentions: msg.mentions,
            message_id: msg.message_id,
        },
    })))
}

fn admit_card(
    action: CardAction,
    policy: &InboundPolicy,
    dedup: &mut Deduper,
) -> Result<GateDecision, Error> {
    let idem = card_idempotency_key(&action);
    if dedup.seen_or_insert(idem.clone()) {
        return Ok(GateDecision::Drop {
            reason: DropReason::Duplicate,
        });
    }
    if !is_owner(&action.operator_id, policy) {
        return Ok(GateDecision::Drop {
            reason: DropReason::OwnerNotAllowed,
        });
    }
    if !policy.chat_allowlist.is_empty() {
        let Some(chat_id) = action.chat_id.as_deref().filter(|id| !id.is_empty()) else {
            return Ok(GateDecision::Drop {
                reason: DropReason::ChatNotAllowed,
            });
        };
        if !policy.chat_allowlist.iter().any(|id| id == chat_id) {
            return Ok(GateDecision::Drop {
                reason: DropReason::ChatNotAllowed,
            });
        }
    }
    let thread = ThreadRef {
        thread_id: None,
        root_id: None,
        reply_to: None,
    };
    let chat_id = action
        .chat_id
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    let callback = action.callback().ok();
    Ok(GateDecision::Take(Box::new(Inbound {
        session_key: SessionKey::from_parts(&chat_id, &thread),
        idempotency_key: idem,
        actor_open_id: action.operator_id.clone(),
        chat_id,
        chat_type: None,
        thread,
        kind: InboundKind::CardAction {
            callback,
            action: Box::new(action),
        },
    })))
}

fn card_idempotency_key(action: &CardAction) -> String {
    if let Some(id) = nonempty(action.event_id.as_deref()) {
        return id.to_string();
    }
    let a = action
        .action_value
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default();
    format!("card:{}:{a}", action.message_id)
}

fn is_owner(open_id: &str, policy: &InboundPolicy) -> bool {
    policy.owner_open_ids.iter().any(|id| id == open_id)
}

fn chat_allowed(chat_id: &str, chat_type: ChatType, policy: &InboundPolicy) -> bool {
    match chat_type {
        ChatType::P2p => {
            policy.chat_allowlist.is_empty() || policy.chat_allowlist.iter().any(|id| id == chat_id)
        }
        ChatType::Group => policy.chat_allowlist.iter().any(|id| id == chat_id),
    }
}

fn mentions_bot(msg: &ImMessage, policy: &InboundPolicy) -> bool {
    let by_id = policy
        .bot_open_id
        .as_deref()
        .is_some_and(|bot| msg.mentions.iter().any(|m| m.id == bot));
    let by_name = policy.bot_name.as_deref().is_some_and(|name| {
        msg.mentions.iter().any(|m| m.name == name)
            || msg
                .content
                .split_whitespace()
                .any(|tok| tok.trim_start_matches('@') == name)
    });
    by_id || by_name
}

fn strip_leading_mentions(content: &str) -> String {
    let mut out = String::new();
    let mut started = false;
    for tok in content.split_whitespace() {
        if !started && (tok.starts_with('@') || tok.starts_with("<at")) {
            continue;
        }
        started = true;
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(tok);
    }
    out
}

fn parse_agent(raw: &str) -> AgentKind {
    match raw.to_ascii_lowercase().as_str() {
        "claude" => AgentKind::Claude,
        "codex" => AgentKind::Codex,
        "grok" => AgentKind::Grok,
        "agy" => AgentKind::Agy,
        _ => AgentKind::Generic,
    }
}

fn parse_callback_value(value: Option<&Value>) -> Result<CallbackValue, Error> {
    let Some(value) = value else {
        return Err(Error::MissingField("action_value"));
    };
    let parsed = match value {
        Value::String(s) => serde_json::from_str::<CallbackValue>(s)?,
        Value::Object(_) => serde_json::from_value::<CallbackValue>(value.clone())?,
        _ => {
            return Err(Error::InvalidAnswer(
                "action_value must be a JSON object or JSON string".into(),
            ));
        }
    };
    if parsed.tid.is_empty() || parsed.a.is_empty() {
        return Err(Error::MissingField("tid/a"));
    }
    Ok(parsed)
}

fn nonempty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

fn empty_to_none(s: Option<String>) -> Option<String> {
    s.and_then(|v| {
        let t = v.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_key_prefers_thread_then_root_then_main() {
        let chat = "oc_chat";
        assert_eq!(
            SessionKey::from_parts(
                chat,
                &ThreadRef {
                    thread_id: Some("omt_t".into()),
                    root_id: Some("om_root".into()),
                    reply_to: None,
                }
            )
            .as_str(),
            "feishu:oc_chat:omt_t"
        );
        assert_eq!(
            SessionKey::from_parts(
                chat,
                &ThreadRef {
                    thread_id: None,
                    root_id: Some("om_root".into()),
                    reply_to: None,
                }
            )
            .as_str(),
            "feishu:oc_chat:om_root"
        );
        assert_eq!(
            SessionKey::from_parts(
                chat,
                &ThreadRef {
                    thread_id: Some(String::new()),
                    root_id: None,
                    reply_to: None,
                }
            )
            .as_str(),
            "feishu:oc_chat:main"
        );
    }

    #[test]
    fn commands_strip_mentions() {
        assert!(matches!(
            parse_intent("@Remuda /status"),
            Intent::Command(ExplicitCommand::Status)
        ));
        assert!(matches!(
            parse_intent("/new"),
            Intent::Command(ExplicitCommand::New)
        ));
        let Intent::Command(ExplicitCommand::Host { name }) = parse_intent("/host devbox")
        else {
            panic!("host");
        };
        assert_eq!(name, "devbox");
        let Intent::Prompt { text } = parse_intent("@Remuda please look at this") else {
            panic!("prompt");
        };
        assert_eq!(text, "please look at this");
    }
}
