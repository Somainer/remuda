//! Minimal ACP v1 client for `grok agent` stdio (D-013 freeze).
//!
//! Speaks **newline-delimited JSON-RPC** (not LSP `Content-Length`). Live
//! connections use the official [`agent_client_protocol`] SDK; fixture replay
//! uses a local codec.
//!
//! Frozen surface: spawn → `initialize` → `session/new` → `session/prompt` +
//! `session/update` stream → `session/cancel`. No `session/load`, no serve/WS.
//!
//! `initialize` sends empty `clientCapabilities` (do not declare `fs` /
//! `terminal`). `--always-approve` means grok does not send
//! `session/request_permission`; if one arrives it is cancelled.

#![cfg_attr(not(test), deny(clippy::unwrap_used))]

mod client;
mod codec;
mod error;
mod spawn;
mod types;

pub use client::{AcpConn, AcpSession, connect_byte_streams, connect_stdio, connect_transport};
pub use codec::{
    MAX_LINE_BYTES, ParsedLine, chunk_text, classify_rpc, classify_session_update,
    encode_notification, encode_request, encode_response, parse_line, parse_value, tool_call_title,
};
pub use error::Error;
pub use spawn::{GrokChild, drain_stderr};
pub use types::{
    CLIENT_NAME, CaptureMeta, DEFAULT_MODEL, Direction, InboundEvent, PromptTurn, SessionSpec,
    SessionUpdateKind, SpawnSpec, TransportKind, WireEvent, adapter_version,
    client_capabilities_declare_fs_or_terminal, grok_binary, initialize_params,
};

pub use agent_client_protocol::schema::ProtocolVersion;
pub use agent_client_protocol::schema::v1::{
    InitializeResponse, NewSessionResponse, SessionId, StopReason,
};
