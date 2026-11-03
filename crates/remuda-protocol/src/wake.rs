//! Wake-ups (cb-wake-engine, D-060 §11): durable rows a lineage writes so the
//! Hub wakes it at a time, at every interval, or when a fact arrives.
//!
//! A wake-up belongs to one lineage. The Hub's writer tick claims each due
//! occurrence once and records a run; a run in `persistent` mode is delivered
//! to the lineage's live chapter as a `wake-due` inbox fact, so it waits for
//! the idle point like any fact, and a run in `fresh` mode makes the Hub
//! create a short-lived child under the lineage instead. The `notify`
//! declaration is the caller's; the Hub never picks one.
//!
//! The text of a wake-up is the lineage's own words (or the owner's, for one
//! the owner added); it is quoted to the lineage as a JSON string.

use crate::{InboxFactKind, InputOrigin};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Characters a wake-up name may hold (from `[A-Za-z0-9._:-]`, not starting
/// with `wak_`: `rm` takes a name or an id in the URL).
pub const WAKE_NAME_MAX_CHARS: usize = 64;
/// Characters a wake-up text may hold.
pub const WAKE_TEXT_MAX_CHARS: usize = 2000;
/// Shortest `every` interval, in seconds.
pub const WAKE_EVERY_MIN_SECS: i64 = 60;
/// Longest interval, and the furthest ahead an `at` or `startAt` may lie,
/// in seconds (one year).
pub const WAKE_MAX_SECS: i64 = 31_536_000;
/// Wake-ups one lineage may hold.
pub const WAKE_MAX_PER_LINEAGE: usize = 64;
/// Runs `GET /v1/lineages/{id}/wake-ups` lists, newest first.
pub const WAKE_RUNS_LISTED: usize = 50;

wire_enum!(WakeMode, "8.4", {
    Persistent => "persistent",
    Fresh => "fresh",
});

impl WakeMode {
    /// Wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Persistent => "persistent",
            Self::Fresh => "fresh",
        }
    }

    /// Parse a wire spelling.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        [Self::Persistent, Self::Fresh]
            .into_iter()
            .find(|mode| mode.as_str() == raw)
    }
}

wire_enum!(WakeNotify, "8.4", {
    Never => "never",
    Failure => "failure",
    Always => "always",
});

impl WakeNotify {
    /// Wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::Failure => "failure",
            Self::Always => "always",
        }
    }

    /// Parse a wire spelling.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        [Self::Never, Self::Failure, Self::Always]
            .into_iter()
            .find(|notify| notify.as_str() == raw)
    }
}

wire_enum!(WakeUpState, "8.4", {
    Active => "active",
    Done => "done",
});

impl WakeUpState {
    /// Wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Done => "done",
        }
    }

    /// Parse a wire spelling.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        [Self::Active, Self::Done]
            .into_iter()
            .find(|state| state.as_str() == raw)
    }
}

wire_enum!(WakeRunState, "8.4", {
    Queued => "queued",
    Running => "running",
    Success => "success",
    Failed => "failed",
    Skipped => "skipped",
    Interrupted => "interrupted",
});

impl WakeRunState {
    /// Every state, in declaration order.
    pub const ALL: [WakeRunState; 6] = [
        Self::Queued,
        Self::Running,
        Self::Success,
        Self::Failed,
        Self::Skipped,
        Self::Interrupted,
    ];

    /// Wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::Interrupted => "interrupted",
        }
    }

    /// Parse a wire spelling.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.as_str() == raw)
    }
}

/// When a wake-up fires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind")]
pub enum WakeTrigger {
    /// Once, at a time.
    #[serde(rename = "at")]
    At(WakeAt),
    /// At every interval.
    #[serde(rename = "every")]
    Every(WakeEvery),
    /// Each time a fact of a kind reaches the lineage's inbox.
    #[serde(rename = "when")]
    When(WakeWhen),
}

impl WakeTrigger {
    /// Wire spelling of the trigger kind.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::At(_) => "at",
            Self::Every(_) => "every",
            Self::When(_) => "when",
        }
    }
}

/// `at`: one occurrence. A request gives `at` or `afterSecs`; a stored
/// wake-up always carries `at`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WakeAt {
    /// The time it fires (UTC RFC3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// Request only: seconds after the Hub received the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_secs: Option<i64>,
}

/// `every`: one occurrence per interval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WakeEvery {
    /// The interval in seconds, at least [`WAKE_EVERY_MIN_SECS`].
    pub every_secs: i64,
    /// The first occurrence (UTC RFC3339); one interval after the add when
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_at: Option<String>,
}

/// `when`: a condition over the facts the lineage receives. Each fact of
/// `factKind` (about `subjectInstanceId`, when set) recorded after the add is
/// an occurrence. `wake-due` cannot be a condition, and nothing records
/// `context-threshold` facts yet, so that kind is refused at add time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WakeWhen {
    /// The fact kind that fires it.
    pub fact_kind: InboxFactKind,
    /// Only facts about this instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_instance_id: Option<String>,
}

/// `POST /v1/lineages/{id}/wake-ups`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WakeAddRequest {
    /// Unique in the lineage (`[A-Za-z0-9._:-]`, not `wak_…`); a repeat of
    /// the same add under the same name returns the stored wake-up. The Hub
    /// generates one when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// What the wake-up is for: delivered with each run, or the brief of a
    /// fresh-mode child.
    pub text: String,
    /// When it fires.
    pub trigger: WakeTrigger,
    /// Reserved for the optional check (main-agent.md §11.2); refused with
    /// 422 `wake-check-unsupported` until it is built.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<serde_json::Value>,
    /// `persistent` (default) or `fresh`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<WakeMode>,
    /// Which runs notify the owner; required, the Hub never picks.
    pub notify: WakeNotify,
}

/// Who added a wake-up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WakeCreator {
    /// `agent` (the lineage itself) or `human` (the owner).
    pub origin: InputOrigin,
    /// The chapter an Agent added it from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
}

/// One wake-up row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WakeUp {
    /// `wak_…`.
    pub wake_id: String,
    /// The lineage it wakes.
    pub lineage_id: String,
    /// Unique in the lineage.
    pub name: String,
    /// What it is for.
    pub text: String,
    /// When it fires (an `at` carries its resolved time).
    pub trigger: WakeTrigger,
    /// Reserved for the optional check (main-agent.md §11.2); always absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<serde_json::Value>,
    /// Delivery mode.
    pub mode: WakeMode,
    /// The declared notify policy.
    pub notify: WakeNotify,
    /// `done` once an `at` has fired.
    pub state: WakeUpState,
    /// The next scheduled occurrence (UTC RFC3339); absent for `when` and
    /// once done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_fire_at: Option<String>,
    /// Occurrences that came due while an earlier run was still queued or
    /// running; the next run reports them and this resets.
    pub missed_count: i64,
    /// When it was added (UTC RFC3339).
    pub created_at: String,
    /// When the Hub last changed it.
    pub updated_at: String,
    /// When its latest run fired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_at: Option<String>,
    /// Who added it.
    pub creator: WakeCreator,
}

/// One run of a wake-up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WakeRun {
    /// `wrn_…`.
    pub run_id: String,
    /// The wake-up (it may have been removed since).
    pub wake_id: String,
    /// The lineage it woke.
    pub lineage_id: String,
    /// The wake-up's name when it fired.
    pub name: String,
    /// The wake-up's mode when it fired.
    pub mode: WakeMode,
    /// The occurrence it stands for: the latest one due when it fired.
    pub scheduled_at: String,
    /// When the Hub claimed it.
    pub fired_at: String,
    /// `queued` → `running` → `success`, `failed` or `skipped`; a run that
    /// was running when the Hub restarted is `interrupted` and never
    /// replayed.
    pub state: WakeRunState,
    /// When it started running: its batch was forwarded, or its fresh child
    /// was being created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// When it ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// The optional check's exit code (main-agent.md §11.2); null until the
    /// check exists.
    pub check_exit: Option<i64>,
    /// The `wake-due` inbox row it was delivered as (persistent mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fact_id: Option<i64>,
    /// The inbox batch that carried it (persistent mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<String>,
    /// The child the Hub created for it (fresh mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_instance_id: Option<String>,
    /// Occurrences missed before it, which it reports.
    pub missed_before: i64,
    /// Why it failed: `not-received`, `not-delivered`, `delivery`,
    /// `turn-failed`, `chapter-ended`, `fresh-refused`, `child-failed`,
    /// `child-ended`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<String>,
}

/// `POST /v1/lineages/{id}/wake-ups` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WakeAddResult {
    /// The stored wake-up.
    pub wake_up: WakeUp,
    /// False when the same add under the same name already stored it.
    pub created: bool,
}

/// `GET /v1/lineages/{id}/wake-ups`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WakeList {
    /// The lineage.
    pub lineage_id: String,
    /// Its wake-ups, oldest first.
    pub items: Vec<WakeUp>,
    /// Its latest runs, newest first, at most [`WAKE_RUNS_LISTED`].
    pub runs: Vec<WakeRun>,
}

/// `DELETE /v1/lineages/{id}/wake-ups/{wake}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WakeRemoved {
    /// The removed wake-up.
    pub wake_id: String,
    /// Its name.
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn triggers_round_trip_with_their_kind_tag() {
        for (wire, trigger) in [
            (
                json!({"kind": "at", "at": "2026-11-04T10:00:00.000Z"}),
                WakeTrigger::At(WakeAt {
                    at: Some("2026-11-04T10:00:00.000Z".into()),
                    after_secs: None,
                }),
            ),
            (
                json!({"kind": "every", "everySecs": 180}),
                WakeTrigger::Every(WakeEvery {
                    every_secs: 180,
                    start_at: None,
                }),
            ),
            (
                json!({"kind": "when", "factKind": "gate"}),
                WakeTrigger::When(WakeWhen {
                    fact_kind: InboxFactKind::Gate,
                    subject_instance_id: None,
                }),
            ),
        ] {
            assert_eq!(serde_json::to_value(&trigger).unwrap(), wire);
            assert_eq!(
                serde_json::from_value::<WakeTrigger>(wire).unwrap(),
                trigger
            );
        }
    }

    #[test]
    fn an_add_must_declare_notify() {
        let missing = json!({"text": "t", "trigger": {"kind": "at", "afterSecs": 60}});
        assert!(serde_json::from_value::<WakeAddRequest>(missing).is_err());
        let declared = json!({"text": "t", "trigger": {"kind": "at", "afterSecs": 60},
                              "notify": "failure"});
        let add: WakeAddRequest = serde_json::from_value(declared).unwrap();
        assert_eq!(add.notify, WakeNotify::Failure);
        assert_eq!(add.mode, None);
    }

    #[test]
    fn states_round_trip_their_wire_spelling() {
        for state in WakeRunState::ALL {
            assert_eq!(WakeRunState::parse(state.as_str()), Some(state));
            assert_eq!(serde_json::to_value(state).unwrap(), json!(state.as_str()));
        }
        assert_eq!(WakeMode::parse("fresh"), Some(WakeMode::Fresh));
        assert_eq!(WakeNotify::parse("always"), Some(WakeNotify::Always));
        assert_eq!(WakeUpState::parse("done"), Some(WakeUpState::Done));
    }
}
