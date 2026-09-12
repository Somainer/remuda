//! Herdr socket API client and `terminal session observe/control` bridge.
//!
//! Remuda uses Herdr as the PTY carrier. This crate talks JSON-RPC over the
//! API Unix socket and spawns `herdr terminal session observe|control` for
//! raw ANSI frames. It does **not** depend on Herdr's agent-resume argv:
//! `agent.start` arguments must be persisted by the caller.

#![cfg_attr(not(test), deny(clippy::unwrap_used))]

mod client;
mod error;
mod events;
mod rpc;
mod server;
mod terminal;
mod types;

pub use client::{Client, EventStream, default_api_socket, herdr_config_dir, session_sockets};
pub use error::Error;
pub use events::{Event, EventKind, EventsSubscribeParams, Subscription, normalize_event_name};
pub use rpc::{Incoming, RpcErrorBody, RpcRequest, parse_line};
pub use server::HerdrServer;
pub use terminal::{
    TerminalCommand, TerminalEnvelope, TerminalFrame, TerminalMode, TerminalObserver, TerminalOpen,
    parse_terminal_line,
};
pub use types::*;
