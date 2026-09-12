//! Feishu dispatcher adapter for Remuda (M2 predecessor).
//!
//! This crate is a channel adapter, not an agent loop. It:
//! - parses `lark-cli event consume` NDJSON (`im.message.receive_v1`, `card.action.trigger`)
//! - builds `session_key = feishu:{chat_id}:{thread_id||root_id||main}`
//! - gates owners, chat allowlists, and group `@bot` mentions
//! - wraps `lark-cli im +messages-send/+messages-reply` (DryRun by default)
//! - renders Card JSON 2.0 templates and maps them onto `protocol.md` Interactions
//! - supervises consume children (stdin kept open, SIGTERM on stop, backoff restart)
//! - maps `session_key` → instanceId and routes `/new` `/host` `/agent` `/model`
//!   `/status` `/stop` `/yes` `/no` through [`InstanceApi`]
//!
//! It does not create Feishu apps, send live messages, or change `lark-cli` config.

mod cards;
mod consume;
mod dispatcher;
mod error;
mod hub_api;
mod inbound;
mod outbound;
mod runtime;
mod tickets;

pub use cards::{
    CardKitOp, CardKitStream, PROGRESS_ELEMENT_ID, card_kind, render_approval_card,
    render_completion_card, render_expired_card, render_interaction_card, render_progress_card,
    render_question_card, render_recorded_card, validate_card,
};
pub use consume::{Backoff, ConsumeEvent, ConsumeSettings, ConsumeSupervisor};
pub use dispatcher::{
    ApiCall, CreateRequest, CreatedInstance, DispatchAction, DispatchReport, Dispatcher,
    FakeInstanceApi, FollowEvent, FollowPage, InstanceApi, RespondRequest, RouteDefaults,
    SendRequest, SessionBinding, SessionStatus, SessionStore,
};
pub use error::Error;
pub use hub_api::HubInstanceApi;
pub use inbound::{
    CallbackValue, CardAction, ChatType, Deduper, DropReason, ExplicitCommand, GateDecision,
    ImMessage, Inbound, InboundKind, InboundPolicy, Intent, Mention, RawEvent, SessionKey,
    ThreadRef, admit, parse_event_line, parse_intent,
};
pub use outbound::{
    ExecutionMode, LarkCli, OutboundBody, OutboundReceipt, OutboundTarget, PlannedCommand,
    idempotency_key,
};
pub use runtime::{DispatcherRun, run_dispatcher};
pub use tickets::{
    DEFAULT_INTERACTION_TTL, MAX_INTERACTION_TTL, MIN_INTERACTION_TTL, MappedAnswer, TicketState,
    TicketStore, parse_form_value,
};

/// Event key for inbound IM (one consume process).
pub const EVENT_IM_RECEIVE: &str = "im.message.receive_v1";
/// Callback key for card buttons/forms (one consume process, `single_consumer`).
pub const EVENT_CARD_ACTION: &str = "card.action.trigger";
