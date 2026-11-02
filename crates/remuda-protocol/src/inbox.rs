//! The Hub inbox of facts (cb-inbox-facts, D-057 §10, D-060 §10.1).
//!
//! A fact is one durable row the Hub writes for a *subscriber lineage* when
//! something it may act on happened to a subject: a child it dispatched, a
//! gate job it enqueued, a host in its scope. Rows are written on the Hub
//! writer thread from journal appends and the Hub's own committed state
//! changes; delivery (batching, the idle gate, the owner-push fallback) is a
//! separate task and only reads these rows.
//!
//! Payload text a worker or tool produced is untrusted data; a reader must not
//! treat it as an instruction or as authorization.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Default occurrence cap per `(subscriber, subject, kind)` in a rolling
/// window (§10.1 item 2); the project setting `factOccurrenceCap` overrides it.
pub const DEFAULT_FACT_OCCURRENCE_CAP: i64 = 6;
/// Length of the rolling occurrence window, in hours (§10.1 item 2).
pub const FACT_OCCURRENCE_WINDOW_HOURS: i64 = 24;
/// Rows `GET /v1/lineages/{id}/inbox` returns when `limit` is absent.
pub const INBOX_PAGE_DEFAULT_LIMIT: u32 = 50;
/// Most rows one inbox page returns.
pub const INBOX_PAGE_MAX_LIMIT: u32 = 200;

wire_enum!(InboxFactKind, "8.4", {
    Started => "started",
    Exited => "exited",
    Waiting => "waiting",
    TurnDone => "turn-done",
    TurnFailed => "turn-failed",
    Interaction => "interaction",
    InteractionSuperseded => "interaction-superseded",
    Gate => "gate",
    SupplyReset => "supply-reset",
    HostOnline => "host-online",
    HostOffline => "host-offline",
    Stalled => "stalled",
    BudgetExpired => "budget-expired",
    WakeDue => "wake-due",
    ContextThreshold => "context-threshold",
});

impl InboxFactKind {
    /// Every kind, in declaration order.
    pub const ALL: [InboxFactKind; 15] = [
        Self::Started,
        Self::Exited,
        Self::Waiting,
        Self::TurnDone,
        Self::TurnFailed,
        Self::Interaction,
        Self::InteractionSuperseded,
        Self::Gate,
        Self::SupplyReset,
        Self::HostOnline,
        Self::HostOffline,
        Self::Stalled,
        Self::BudgetExpired,
        Self::WakeDue,
        Self::ContextThreshold,
    ];

    /// Wire spelling; also the first segment of every dedupe key.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Exited => "exited",
            Self::Waiting => "waiting",
            Self::TurnDone => "turn-done",
            Self::TurnFailed => "turn-failed",
            Self::Interaction => "interaction",
            Self::InteractionSuperseded => "interaction-superseded",
            Self::Gate => "gate",
            Self::SupplyReset => "supply-reset",
            Self::HostOnline => "host-online",
            Self::HostOffline => "host-offline",
            Self::Stalled => "stalled",
            Self::BudgetExpired => "budget-expired",
            Self::WakeDue => "wake-due",
            Self::ContextThreshold => "context-threshold",
        }
    }

    /// Parse a wire spelling.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == raw)
    }
}

/// How often the same fact occurred for this subscriber in the rolling window
/// ending at the fact's `stateEnteredAt` (§10.1 item 2). The count survives a
/// Hub restart; occurrences over the cap are counted but write no row, so the
/// next recorded fact carries them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InboxOccurrence {
    /// Occurrences in the window, this one included.
    pub count: i64,
    /// Occurrences in the window the cap kept out of the inbox.
    pub capped: i64,
    /// The cap in force when the fact was written.
    pub cap: i64,
    /// Window length in hours.
    pub window_hours: i64,
}

/// One inbox row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InboxFact {
    /// Row id; the `after` cursor of the read route.
    pub id: i64,
    /// Lineage the fact is for.
    pub lineage_id: String,
    /// What happened.
    pub kind: InboxFactKind,
    /// `kind:subject:runGeneration:stateEnteredAt`; unique in the
    /// subscriber's inbox and stable across a Hub restart and a replayed
    /// journal append.
    pub dedupe_key: String,
    /// The instance the fact is about, when it is about one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_instance_id: Option<String>,
    /// Run generation of the subject the fact is about (its root-turn count
    /// when the state was entered); `0` for facts not about a run.
    pub run_generation: i64,
    /// When the subject entered the state: journal time for journal facts,
    /// the Hub's commit time for Hub-observed ones (UTC RFC3339, ms).
    pub state_entered_at: String,
    /// When the Hub wrote the row (UTC RFC3339).
    pub created_at: String,
    /// The delivery batch that carried the row; absent until one does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<String>,
    /// Kind-specific detail. Worker- and tool-produced text in it is
    /// untrusted data.
    pub payload: serde_json::Value,
}

/// `GET /v1/lineages/{id}/inbox`: one page of a lineage's inbox, oldest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InboxPage {
    /// Lineage the page is for.
    pub lineage_id: String,
    /// Rows with `id > after`, ascending, at most `limit`.
    pub items: Vec<InboxFact>,
    /// The cursor for the next page: the last row's id, or the request's
    /// `after` when the page is empty.
    pub next_after: i64,
    /// Whether rows beyond this page exist.
    pub more: bool,
    /// The lineage's subscription settings.
    pub settings: InboxSettings,
}

/// A lineage's inbox settings (§10, Defaults).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InboxSettings {
    /// A Human-created lineage without continuity receives facts only when
    /// its creator opted in. Continuity lineages and agent-created
    /// coordinators receive them regardless.
    pub opt_in: bool,
    /// Kinds the lineage does not receive.
    pub muted_kinds: Vec<InboxFactKind>,
}

/// `PATCH /v1/lineages/{id}/inbox`: absent fields keep their value.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InboxSettingsPatch {
    /// Owner opt-in; Human devices only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opt_in: Option<bool>,
    /// The full set of muted kinds (replaces the stored set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub muted_kinds: Option<Vec<InboxFactKind>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip_their_wire_spelling() {
        for kind in InboxFactKind::ALL {
            let wire = serde_json::to_value(kind).unwrap();
            assert_eq!(wire, serde_json::json!(kind.as_str()));
            assert_eq!(InboxFactKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(InboxFactKind::parse("nope"), None);
    }
}
