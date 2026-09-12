//! ACP client for `grok agent` stdio and `grok agent serve` WebSocket.
//!
//! This crate speaks stable ACP v1 as **newline-delimited JSON-RPC** (not LSP
//! `Content-Length`). It uses the official [`agent_client_protocol`] SDK for
//! live connections and a local codec for fixture replay.
//!
//! ## Grok-specific rules
//!
//! - Spawn: `grok agent --always-approve --model grok-4.6 --no-leader stdio`
//!   with `GROK_DISABLE_AUTOUPDATER=1`.
//! - `initialize` sends `protocolVersion: 1` and **empty** `clientCapabilities`
//!   (do not declare `fs` / `terminal`, so tools run in the agent process).
//! - `session/new` uses `{cwd, mcpServers: [], _meta: {yoloMode: true}}` when
//!   always-approve is requested.
//! - `_x.ai/*` extensions keep the leading underscore (document `x.ai/` is
//!   rewritten). Unknown notifications become [`WireEvent::Unknown`].
//! - `--always-approve` means grok does not send `session/request_permission`;
//!   if one arrives it is cancelled.
//!
//! Headless `grok -p --output-format streaming-json` is a **different**
//! protocol and is not driven by this client.

#![cfg_attr(not(test), deny(clippy::unwrap_used))]

mod client;
mod codec;
mod error;
mod spawn;
mod types;
mod ws;

pub use client::{
    AcpConn, AcpSession, connect_byte_streams, connect_stdio, connect_transport, connect_ws,
};
pub use codec::{
    MAX_LINE_BYTES, ParsedLine, chunk_text, classify_rpc, classify_session_update,
    encode_notification, encode_request, encode_response, parse_line, parse_value, tool_call_title,
};
pub use error::Error;
pub use spawn::{GrokChild, drain_stderr};
pub use types::{
    CLIENT_NAME, CaptureMeta, DEFAULT_MODEL, Direction, InboundEvent, PromptTurn, ServeSpec,
    SessionSpec, SessionUpdateKind, SpawnSpec, TransportKind, WireEvent, adapter_version,
    client_capabilities_declare_fs_or_terminal, ensure_ext_method, grok_binary, initialize_params,
    initialize_request,
};
pub use ws::{connect_ws_transport, websocket_request};

pub use agent_client_protocol::schema::ProtocolVersion;
pub use agent_client_protocol::schema::v1::{
    InitializeResponse, LoadSessionResponse, NewSessionResponse, SessionId, StopReason,
};
