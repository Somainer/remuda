//! D-057 §7.6: the `lineage.fence` Hub→Node method, its acknowledgement, and
//! the `lineage` create/resume field (protocol.md §7.2, D-057 additive table).
//!
//! All of it is additive. A Node advertises support with the hello capability
//! [`LINEAGE_FENCE_CAPABILITY`]; the Hub never sends a fence, nor any work a
//! continuity lineage initiated, to a Node without it.

use serde::{Deserialize, Deserializer, Serialize};

/// Hub→Node method name: persist and apply a lineage fence.
pub const METHOD_LINEAGE_FENCE: &str = "lineage.fence";

/// Hello `capabilities` key a fence-capable Node sets to `true`.
pub const LINEAGE_FENCE_CAPABILITY: &str = "lineageFence";

/// Stable refusal reason for work from a fenced initiator (protocol.md §9).
pub const FENCED_REASON: &str = "fenced";

/// Stable refusal reason when the target Node lacks fence support.
pub const NODE_LACKS_FENCE_REASON: &str = "node-lacks-fence";

/// Read a generation sent either as the canonical decimal string (`U64`) or
/// as a JSON number; older Hub builds stamped plain numbers.
fn lenient_generation<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Number(u64),
        Text(String),
    }
    match Raw::deserialize(deserializer)? {
        Raw::Number(value) => Ok(value),
        Raw::Text(text) => text
            .parse()
            .map_err(|_| serde::de::Error::custom("generation must be a decimal integer")),
    }
}

fn generation_string<S: serde::Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&value.to_string())
}

/// `lineage.fence` params: `{fenceId, lineageId, liveGeneration}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LineageFenceParams {
    /// Durable Hub fence id; the Node persists the fence idempotently by it.
    pub fence_id: String,
    /// Lineage the fence applies to (its first chapter's id).
    pub lineage_id: String,
    /// The lineage's live generation after the fence. Every instance and
    /// initiator of the lineage below it is fenced.
    #[serde(
        serialize_with = "generation_string",
        deserialize_with = "lenient_generation"
    )]
    #[schemars(with = "String")]
    pub live_generation: u64,
}

/// What the Node established about one operation it holds a record of
/// (main-agent.md §7.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum FenceOutcome {
    /// Not yet delivered when the fence applied; the Node cancelled it.
    Cancelled,
    /// It reached its executor before the fence applied.
    Ran,
    /// It arrived after the fence applied and was refused at entry.
    Refused,
    /// The Node holds a record but cannot establish whether it reached its
    /// executor (typically a crash between durable intent and evidence).
    Unknown,
}

impl FenceOutcome {
    /// Wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Ran => "ran",
            Self::Refused => "refused",
            Self::Unknown => "unknown",
        }
    }
}

/// One operation outcome inside a fence acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FenceOpOutcome {
    /// Command id, direct-operation id, or gate job id.
    pub op_id: String,
    /// What the Node established.
    pub outcome: FenceOutcome,
}

/// `lineage.fence` result. An operation it does not mention stays unknown.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LineageFenceResult {
    /// Echo of the applied fence id.
    #[serde(default)]
    pub fence_id: String,
    /// Local instances of the lineage below the live generation the Node
    /// closed while applying this fence.
    #[serde(default)]
    pub closed_instance_ids: Vec<String>,
    /// Per-operation outcomes for operations the Node holds a record of.
    #[serde(default)]
    pub outcomes: Vec<FenceOpOutcome>,
}

/// `lineage` on `instance.create` / `instance.resume` params: the instance's
/// lineage and generation, so the Node can close it when a fence names a
/// higher live generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LineageRef {
    /// Lineage id.
    pub id: String,
    /// The chapter's generation.
    #[serde(
        serialize_with = "generation_string",
        deserialize_with = "lenient_generation"
    )]
    #[schemars(with = "String")]
    pub generation: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fence_params_round_trip_with_a_string_generation_and_accept_a_number() {
        let params = LineageFenceParams {
            fence_id: "fen_1".into(),
            lineage_id: "ins_l".into(),
            live_generation: 3,
        };
        let wire = serde_json::to_value(&params).unwrap();
        assert_eq!(
            wire,
            json!({"fenceId":"fen_1","lineageId":"ins_l","liveGeneration":"3"})
        );
        let numeric: LineageFenceParams = serde_json::from_value(
            json!({"fenceId":"fen_1","lineageId":"ins_l","liveGeneration":3}),
        )
        .unwrap();
        assert_eq!(numeric, params);
    }

    #[test]
    fn fence_result_defaults_missing_lists_to_empty() {
        let result: LineageFenceResult = serde_json::from_value(json!({"accepted": true})).unwrap();
        assert!(result.outcomes.is_empty() && result.closed_instance_ids.is_empty());
        let full: LineageFenceResult = serde_json::from_value(json!({
            "fenceId": "fen_1",
            "closedInstanceIds": ["ins_a"],
            "outcomes": [{"opId": "cmd_1", "outcome": "cancelled"}, {"opId": "op", "outcome": "unknown"}]
        }))
        .unwrap();
        assert_eq!(full.outcomes[0].outcome, FenceOutcome::Cancelled);
        assert_eq!(full.outcomes[1].outcome, FenceOutcome::Unknown);
    }
}
