//! Versioned entities, observations, and Hub–Node wire types; `protocol.md` §§1–9.
//!
//! This crate defines data and wire validation, not execution, leases, or a journal.

#[macro_use]
mod macros;
mod binary;
mod capabilities;
mod effort;
mod entities;
mod enums;
mod error;
pub mod hubnode;
mod interaction;
mod json;
mod launch;
mod native;
mod observation;
pub mod path_guard;
mod rpc;
mod scalar;
pub mod schema;

pub use binary::*;
pub use capabilities::*;
pub use effort::*;
pub use entities::*;
pub use enums::*;
pub use error::*;
pub use interaction::*;
pub use json::*;
pub use launch::*;
pub use native::*;
pub use observation::*;
pub use rpc::*;
pub use scalar::*;

/// Hub–Node protocol version implemented by these types; `protocol.md` §7.1.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion { major: 1, minor: 0 };
