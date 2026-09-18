//! Claude Code stream-json duplex adapter (`claude -p` NDJSON control protocol).
//!
//! This crate frames stdin/stdout, not Hub–Node RPC or the driver state machine.
//! Unknown `type` / `subtype` values become `Unknown(Value)` and never fail decode.

mod codec;
mod error;
mod process;
mod types;

pub use codec::{
    DEFAULT_MAX_LINE_BYTES, encode_line, read_inbound, read_outbound, read_raw_line, read_value,
    write_line,
};
pub use error::Error;
pub use process::{ClaudeProcess, SettingSources, SettingsArg, SpawnMode, SpawnSpec};
pub use types::*;
