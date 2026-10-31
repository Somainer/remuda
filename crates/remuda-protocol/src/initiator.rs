//! D-057 §7.1: the Hub-stamped initiator of an Agent-initiated mutation.
//!
//! The Hub derives it from the authenticating device's bound chapter and
//! forwards it to Nodes; the authenticating device id stays Hub-only and is
//! not part of this type. Additive on the wire: older Hubs send no initiator
//! and Nodes treat absent initiators as Hub/Human-origin work.

use serde::{Deserialize, Deserializer, Serialize};

use crate::InputOrigin;

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
    /// D-060 decision 1 (cb-rootturn): the root turn the initiating instance
    /// was in when the Hub admitted the call. Derived by the Hub from its own
    /// turn records, never read from a request body, and read by no admission
    /// rule: it is a record, never a gate (owner ruling R2). Absent when the
    /// Hub knows no turn for the instance, and on initiators older Hubs sent.
    ///
    /// Lenient on read: a value this build cannot parse (a newer origin or
    /// continuation) reads as absent rather than dropping the whole
    /// initiator, which the Node's fence check depends on.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_root_turn"
    )]
    pub root_turn: Option<RootTurn>,
}

impl Initiator {
    /// An initiator with no recorded root turn.
    #[must_use]
    pub fn new(instance_id: String, lineage_id: String, generation: i64) -> Self {
        Self {
            instance_id,
            lineage_id,
            generation,
            root_turn: None,
        }
    }
}

/// D-060 decision 1: the root turn an Agent call was made in.
///
/// `commandId` is the Hub command whose input started the turn (or, for a
/// continuation, the input whose intent it continues). `origin` is who
/// authored that input: an owner's send is `human`; a turn with no new input
/// of its own, or one the Hub authored, is `agent` until ma-restart adds the
/// `hub` origin. `continuation` says how exact the attribution is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RootTurn {
    /// Hub command id of the input that started (or is continued by) the turn.
    pub command_id: String,
    /// Who authored that input.
    pub origin: InputOrigin,
    /// Whether the turn is that input's own turn, or a continuation of it.
    pub continuation: RootTurnContinuation,
}

wire_enum!(RootTurnContinuation, "8.4", {
    // The carrier reported the turn's exact boundaries and this turn is the
    // one `commandId` started (claude-sdk / claude-print).
    Fresh => "none",
    // The carrier started a turn with no new input (for example a background
    // task finished); `commandId` is the input it continues.
    Native => "native",
    // The Hub authored the input (no human, bot or agent caller sent it).
    Hub => "hub",
    // The carrier reports no turn boundaries; `commandId` is the latest input
    // delivered, and the call may belong to a later self-started turn.
    Unknown => "unknown",
});

fn lenient_root_turn<'de, D: Deserializer<'de>>(d: D) -> Result<Option<RootTurn>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(value).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn root_turn_is_additive_and_lenient() {
        let bare: Initiator = serde_json::from_value(json!({
            "instanceId": "ins_a", "lineageId": "ins_a", "generation": 1
        }))
        .expect("an initiator without rootTurn still parses");
        assert_eq!(bare.root_turn, None);
        assert_eq!(
            serde_json::to_value(&bare).unwrap(),
            json!({"instanceId": "ins_a", "lineageId": "ins_a", "generation": 1}),
            "an absent rootTurn is not serialized"
        );

        let stamped: Initiator = serde_json::from_value(json!({
            "instanceId": "ins_a", "lineageId": "ins_a", "generation": 1,
            "rootTurn": {"commandId": "cmd_1", "origin": "human", "continuation": "none"}
        }))
        .unwrap();
        assert_eq!(
            stamped.root_turn,
            Some(RootTurn {
                command_id: "cmd_1".into(),
                origin: InputOrigin::Human,
                continuation: RootTurnContinuation::Fresh,
            })
        );

        let future: Initiator = serde_json::from_value(json!({
            "instanceId": "ins_a", "lineageId": "ins_a", "generation": 1,
            "rootTurn": {"commandId": "cmd_1", "origin": "hub", "continuation": "wake"}
        }))
        .expect("an unknown rootTurn value never drops the initiator");
        assert_eq!(future.root_turn, None);
        assert_eq!(future.instance_id, "ins_a");
    }
}
