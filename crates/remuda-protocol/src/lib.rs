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
mod gate;
pub mod hubnode;
mod interaction;
mod json;
mod launch;
mod model;
mod native;
mod observation;
mod permission;
pub mod path_guard;
mod project;
mod rpc;
mod scalar;
pub mod schema;
mod supply;
mod task;
mod task_notification;
mod worker;

pub use binary::*;
pub use capabilities::*;
pub use effort::*;
pub use entities::*;
pub use enums::*;
pub use error::*;
pub use gate::*;
pub use interaction::*;
pub use json::*;
pub use launch::*;
pub use model::*;
pub use native::*;
pub use permission::*;
pub use observation::*;
pub use project::*;
pub use rpc::*;
pub use scalar::*;
pub use supply::*;
pub use task::*;
pub use task_notification::*;
pub use worker::*;

/// Hub policy default for delegation depth (edges from a human root); §2.5 ⑤.
pub const DEFAULT_MAX_DELEGATION_DEPTH: u32 = 3;
/// Hub policy default for one node's active fan-out (active children); §2.5 ⑤.
pub const DEFAULT_COORDINATOR_FAN_OUT: u32 = 8;

/// Hub–Node protocol version implemented by these types; `protocol.md` §7.1.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion { major: 1, minor: 0 };
