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

pub mod bus;
pub mod event;
pub mod map;
pub mod socket;

pub use bus::{BusContext, SessionBinding, SignalBus};
pub use event::{HookDecision, HookEnvelope, HookEvent, HookReply};
pub use map::{Mapped, MappedKind, map_event};
pub use socket::{HookServer, SignalSink, SocketError, send_event};

/// How long a blocking hook waits for a decision before falling back.
///
/// Aligned with the interaction broker's own TTL
/// (`remuda_driver::interaction::DEFAULT_TTL`, 15 min): a hook that gives up
/// sooner than the broker would retire the ticket turns a decision the user
/// *did* make into a silent fallback. P1 never answers, so this only bounds
/// the observe-only path; P5 makes it load-bearing.
pub const BLOCKING_WAIT: std::time::Duration = std::time::Duration::from_secs(15 * 60);
