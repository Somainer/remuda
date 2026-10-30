//! D-057 §7.1: the Hub-stamped initiator of an Agent-initiated mutation.
//!
//! The Hub derives it from the authenticating device's bound chapter and
//! forwards it to Nodes; the authenticating device id stays Hub-only and is
//! not part of this type. Additive on the wire: older Hubs send no initiator
//! and Nodes treat absent initiators as Hub/Human-origin work.

use serde::{Deserialize, Serialize};

/// The chapter instance a mutation was initiated by, with its place in the
/// lineage. `{instanceId, lineageId, generation}` — D-057 §7.1.
///
/// The generation is the chapter generation stamped at authentication time.
/// A fence bumps the lineage's live generation, so the commit-time check can
/// refuse a request authenticated before the fence (main-agent.md §7.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Initiator {
    /// The chapter instance that initiated the mutation.
    pub instance_id: String,
    /// Lineage the instance belongs to.
    pub lineage_id: String,
    /// Chapter generation at authentication time.
    pub generation: i64,
}
