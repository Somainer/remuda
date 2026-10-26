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
pub use process::{
    ClaudeProcess, ProcessWriter, SettingSources, SettingsArg, SpawnMode, SpawnSpec,
};
/// Test-stub writer gate (ma-sdk-state r9 item 3).
#[cfg(feature = "test-stub")]
pub mod write_gate {
    pub use crate::process::write_gate::WriteGate;
}
pub use types::*;
