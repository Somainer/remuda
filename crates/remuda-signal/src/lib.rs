//! Harness signal adapters (D-028 §4.2).
//!
//! A harness tells us what it is doing over four channels, ranked
//! `Hook > File > OSC > Screen`. This crate owns the top one: the per-instance
//! hook socket, the events that arrive on it, and their translation into
//! [`Observation`](remuda_protocol::Observation)s carrying
//! [`SourceChannel::Hook`](remuda_protocol::SourceChannel::Hook).
//!
//! The transport is deliberately small. A hook fires as a short-lived child of
//! the agent, writes one JSON line, and reads one JSON line back; nothing in
//! it is durable, and the socket dies with the instance directory. Everything
//! that decides *meaning* lives in [`map`], which is pure and therefore
//! testable against payloads recorded from a real `claude` run
//! (`crates/remuda-testing/fixtures/hooks/`).

#![forbid(unsafe_code)]

pub mod approval;
pub mod bus;
pub mod decision;
pub mod event;
pub mod hook_silence;
pub mod live;
pub mod map;
pub mod pending;
pub mod question;
pub mod runtime_dir;
pub mod socket;

pub use approval::{ApprovalContext, approval_interaction, elicitation_interaction};
pub use bus::{BusContext, SessionBinding, SignalBus};
pub use decision::{
    ElicitationAction, HookDecision, PermissionRequestEvent, PermissionSuggestion,
    decision_behavior,
};
pub use event::{HookEnvelope, HookEvent, HookReply};
pub use live::{LiveFold, LiveState, Phase, tool_node_id};
pub use map::{Mapped, MappedKind, map_event};
pub use pending::{DecisionKey, Outcome, PendingDecisions, RetireReason};
pub use question::{
    ASK_USER_QUESTION, answer_from_harness, is_ask_user_question, question_decision,
    question_interaction, question_request, resolved_in_terminal,
};
pub use socket::{Delivery, HookServer, SignalSink, SocketError, deliver_event, send_event};

/// How long a blocking hook waits for a decision before falling back.
///
/// Aligned with the interaction broker's own TTL
/// (`remuda_driver::interaction::DEFAULT_TTL`, 15 min): a hook that gives up
/// sooner than the broker would retire the ticket turns a decision the user
/// *did* make into a silent fallback.
///
/// Note the harness has a bound of its own — claude 2.1.221 defaults to
/// 600 000 ms per hook — so in practice the agent stops waiting first unless
/// the registration raises it. That is why a timeout is a *deny* the relay
/// prints (§4.4) and not merely a give-up: whoever stops waiting first, the
/// tool must not run unapproved.
pub const BLOCKING_WAIT: std::time::Duration = std::time::Duration::from_secs(15 * 60);
